//! Root-only `GET /v2/sessions/{id}/history?limit=&cursor=`: a session's
//! stored native thread history in the `thread/read` (`includeTurns`) shape,
//! available while the runtime is offline.
//!
//! - Runtime connected: proxied to the live app-server, so the in-memory
//!   active turn is included.
//! - Otherwise: read from the stored rollout by an isolated, read-only
//!   app-server on the session's own native home (the bin-side opener, the
//!   same launch `cutex session history` uses; for a Claude agent the
//!   adapter's read-only `cute_claude.history_reader`), kept warm for a short while
//!   so paging is cheap. Only `thread/read`, `thread/turns/list` and
//!   `thread/items/list` are sent; no thread is resumed, started or written. When the native
//!   runtime does not know `thread/turns/list`, it falls back to
//!   `thread/read includeTurns` and pages locally. The reader is stopped as
//!   soon as the runtime is seen live (or an offline submit starts it).
//!
//! Success body:
//! `{ contractVersion: 2, cutexSessionId, threadId, source: "live"|"offline",
//!    native: { method: "thread/read", result: { thread: { ..., turns } } },
//!    page: { limit, cursor, nextCursor, hasMore, firstOrdinal, turnCount },
//!    cutexState: { latestPlan, latestTokenUsage } }`
//! where `turns` are the newest `limit` turns older than `cursor`, oldest
//! first; pass `nextCursor` back as `cursor` to load older turns.
//! `cutexState.latestPlan` is the bootstrap's value (`launch::native_plan`,
//! derived from the stored rollout for live and offline sources alike);
//! `cutexState.latestTokenUsage` likewise (`launch::native_token_usage`).
//!
//! history-pages: `GET /v2/sessions/{id}/bootstrap?turnLimit=N` reads its
//! live snapshot with `read_history_page` too (newest N turns + `historyPage`);
//! clients then page older turns here with `cursor=<nextCursor>`.
//! `GET .../history/turns/{turnId}` reads one bounded preview; internal mutation
//! consumers must use `read_complete_turn` (per session: `read_complete_session_turn`),
//! which fails on incomplete detail.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::json;
use serde_json::Value;

use super::codex_config::CodexConfigHost;
use super::codex_config::CodexEndpointFactory;
use super::codex_config::SpawnedEndpoint;
use super::turn_index::{self, Key};
use crate::catalog::CatalogError;
use crate::http::server::SimpleHttpRequest;
use crate::management::server::ManagementNativeForwardError;
use crate::management::server::ManagementNativeRequestHandler;
pub use crate::management::server::OfflineHistoryEndpointOpener;

const READER_KEEP_WARM: Duration = Duration::from_secs(60);
pub const DEFAULT_LIMIT: u32 = 20;
pub const MAX_LIMIT: u32 = 200;
const OFFSET_CURSOR_PREFIX: &str = "cutex-offset:";
const MAX_CURSOR_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryQuery {
    pub limit: u32,
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HistoryCallError {
    /// The native app-server answered with a JSON-RPC error object.
    Rpc(Value),
    /// No usable app-server (not started, transport failure, timeout).
    Unavailable(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct HistoryPage {
    /// Native `Thread` with `turns` set to this page (oldest first).
    pub thread: Value,
    pub next_cursor: Option<String>,
    pub first_ordinal: Option<usize>,
    pub turn_count: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HistoryResponse {
    pub status: u16,
    pub reason: &'static str,
    /// Success body, or `{code, message, retryable, details}` for an error
    /// the caller wraps in the usual v2 error envelope.
    pub body: Value,
}

impl HistoryResponse {
    pub fn is_success(&self) -> bool {
        self.status == 200
    }
}

pub fn parse_query(request: &SimpleHttpRequest) -> Result<HistoryQuery, String> {
    let params = super::workspace::query_params(request);
    if let Some(key) = params
        .keys()
        .find(|key| !matches!(key.as_str(), "limit" | "cursor" | "compact"))
    {
        return Err(format!("unknown history query parameter: {key}"));
    }
    let limit = match params.get("limit") {
        None => DEFAULT_LIMIT,
        Some(value) => value
            .parse::<u32>()
            .ok()
            .filter(|limit| (1..=MAX_LIMIT).contains(limit))
            .ok_or_else(|| format!("limit must be an integer between 1 and {MAX_LIMIT}"))?,
    };
    let cursor = match params.get("cursor") {
        None => None,
        Some(cursor) if cursor.is_empty() => None,
        Some(cursor) if cursor.len() > MAX_CURSOR_BYTES => {
            return Err("cursor is too long".to_string())
        }
        Some(cursor) => Some(cursor.clone()),
    };
    Ok(HistoryQuery { limit, cursor })
}

/// `GET /v2/sessions/{id}/bootstrap?turnLimit=N` (history-pages): `None`
/// (no turnLimit) keeps the full `thread/read includeTurns` snapshot. Other
/// query parameters are ignored, as they always were on this route.
pub fn parse_bootstrap_turn_limit(request: &SimpleHttpRequest) -> Result<Option<u32>, String> {
    match super::workspace::query_params(request).get("turnLimit") {
        None => Ok(None),
        Some(value) => value
            .parse::<u32>()
            .ok()
            .filter(|limit| (1..=MAX_LIMIT).contains(limit))
            .map(Some)
            .ok_or_else(|| format!("turnLimit must be an integer between 1 and {MAX_LIMIT}")),
    }
}

/// `{limit, cursor, nextCursor, hasMore}` of one page (history body `page`,
/// bootstrap `historyPage`).
pub fn page_json(limit: u32, cursor: Option<&str>, page: &HistoryPage) -> Value {
    json!({
        "limit": limit,
        "cursor": cursor,
        "nextCursor": page.next_cursor,
        "hasMore": page.next_cursor.is_some(),
        "firstOrdinal": page.first_ordinal,
        "turnCount": page.turn_count,
    })
}

/// Adapt a raw JSON-RPC forwarder (`request -> response`, as the bootstrap
/// uses) to `read_history_page`'s `call(method, params) -> result`.
pub fn rpc_call<'a>(
    forward: &'a mut dyn FnMut(Value) -> Result<Value, String>,
) -> impl FnMut(&str, Value) -> Result<Value, HistoryCallError> + 'a {
    move |method, params| {
        let response = forward(json!({
            "id": format!("cutex-history-{}", uuid::Uuid::new_v4()),
            "method": method,
            "params": params,
        }))
        .map_err(HistoryCallError::Unavailable)?;
        if let Some(error) = response.get("error") {
            return Err(HistoryCallError::Rpc(error.clone()));
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| HistoryCallError::Unavailable("native response has no result".into()))
    }
}

/// Read one history page with `call(method, params) -> native result`.
pub fn read_history_page(
    thread_id: &str,
    query: &HistoryQuery,
    call: &mut dyn FnMut(&str, Value) -> Result<Value, HistoryCallError>,
) -> Result<HistoryPage, HistoryCallError> {
    let metadata = call(
        "thread/read",
        json!({ "threadId": thread_id, "includeTurns": false }),
    )?;
    let mut thread = metadata
        .get("thread")
        .filter(|thread| thread.is_object())
        .cloned()
        .ok_or_else(|| HistoryCallError::Unavailable("thread/read returned no thread".into()))?;
    if thread.get("id").and_then(Value::as_str) != Some(thread_id) {
        return Err(HistoryCallError::Unavailable(
            "thread/read returned a different thread".into(),
        ));
    }
    let offset_cursor = match query.cursor.as_deref() {
        Some(cursor) => match cursor.strip_prefix(OFFSET_CURSOR_PREFIX) {
            Some(offset) => Some(offset.parse::<usize>().map_err(|_| {
                HistoryCallError::Rpc(
                    json!({ "code": -32602, "message": "invalid history cursor" }),
                )
            })?),
            None => None,
        },
        None => Some(0),
    };
    let native_cursor = query
        .cursor
        .as_deref()
        .filter(|cursor| !cursor.starts_with(OFFSET_CURSOR_PREFIX));
    let listed = if query.cursor.is_none() || native_cursor.is_some() {
        match call(
            "thread/turns/list",
            json!({
                "threadId": thread_id,
                "cursor": native_cursor,
                "limit": query.limit,
                "sortDirection": "desc",
                "itemsView": "full",
            }),
        ) {
            Ok(page) => Some(page),
            // A native cursor only comes from thread/turns/list itself, so a
            // failure there is real; without one, assume an older runtime.
            Err(error) if native_cursor.is_none() && turn_index::unsupported(&error) => None,
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    let mut first_ordinal = None;
    let mut turn_count = None;
    let (turns, next_cursor) = match listed {
        Some(page) => {
            let mut turns = page
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| {
                    HistoryCallError::Unavailable("thread/turns/list returned no data".into())
                })?;
            turns.reverse();
            let next = page
                .get("nextCursor")
                .and_then(Value::as_str)
                .filter(|cursor| !cursor.is_empty())
                .map(str::to_string);
            (turns, next)
        }
        None => {
            let offset = offset_cursor.unwrap_or(0);
            let full = call(
                "thread/read",
                json!({ "threadId": thread_id, "includeTurns": true }),
            )?;
            let all = full
                .pointer("/thread/turns")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let end = all.len().saturating_sub(offset);
            let start = end.saturating_sub(query.limit as usize);
            first_ordinal = (start < end).then_some(start + 1);
            turn_count = Some(all.len());
            let next =
                (start > 0).then(|| format!("{OFFSET_CURSOR_PREFIX}{}", offset + (end - start)));
            (all[start..end].to_vec(), next)
        }
    };
    thread
        .as_object_mut()
        .expect("thread is an object")
        .insert("turns".to_string(), Value::Array(turns));
    Ok(HistoryPage {
        thread,
        next_cursor,
        first_ordinal,
        turn_count,
    })
}

pub fn index_key(session_id: &str, session: &Value, source: &str) -> Key {
    Key::new(
        session_id,
        session
            .pointer("/native/threadId")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        source,
        session
            .pointer("/runtime/runtimeGeneration")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    )
}

/// Annotate a newest bootstrap page without any extra RPC or cold-history scan.
pub fn annotate_bootstrap(key: &Key, page: &mut HistoryPage) {
    if page.turn_count.is_some() {
        return;
    } // Legacy full-read already counted.
    if let Some(turns) = page.thread["turns"].as_array() {
        if let Ok(Some(index)) = turn_index::observe_newest(key, turns, page.next_cursor.is_some())
        {
            (page.first_ordinal, page.turn_count) = index.ordinals(turns);
        }
    }
}

/// One `/history` page with whatever ordinals are known without extra native
/// calls (PERF-10): a native `thread/turns/list` call replays the whole
/// rollout of a legacy thread, so building a cold index here cost one replay
/// per 100 turns on every first page read. The newest page reconciles the
/// cached index like the bootstrap does (exact `firstOrdinal` / `turnCount`
/// when the page reaches the start of the thread or overlaps a cached index);
/// an older page reports `firstOrdinal` from the cached index (positions
/// before a page turn only change by a rebuild) and no `turnCount`, which
/// only the newest page can confirm. Single-turn reads still build the index.
fn read_indexed_page(
    key: &Key,
    thread_id: &str,
    query: &HistoryQuery,
    call: &mut dyn FnMut(&str, Value) -> Result<Value, HistoryCallError>,
) -> Result<HistoryPage, HistoryCallError> {
    let mut page = read_history_page(thread_id, query, call)?;
    if page.turn_count.is_some() {
        return Ok(page); // Legacy full read already counted.
    }
    if query.cursor.is_none() {
        annotate_bootstrap(key, &mut page);
    } else if let Some(index) = turn_index::cached(key) {
        let turns = page.thread["turns"].as_array().unwrap();
        if !turns.is_empty() {
            page.first_ordinal = index.ordinals(turns).0;
        }
    }
    Ok(page)
}

pub fn parse_turn_query(request: &SimpleHttpRequest) -> Result<(), String> {
    if let Some(key) = super::workspace::query_params(request)
        .keys()
        .find(|key| key.as_str() != "compact")
    {
        return Err(format!("unknown history turn query parameter: {key}"));
    }
    Ok(())
}

/// HTTP previews retain at most 512 KiB of turn detail. Complete internal reads
/// allow 16 MiB and fail closed on any limit or native truncation.
pub const PREVIEW_BYTES: usize = 512 * 1024;
const COMPLETE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug)]
pub struct CompleteHistoryTurn {
    thread: Value,
    pub ordinal: usize,
    pub turn_count: usize,
}

impl CompleteHistoryTurn {
    pub fn thread(&self) -> &Value {
        &self.thread
    }
    pub fn turn(&self) -> &Value {
        &self.thread["turns"][0]
    }
}

/// Reuse point for internal history consumers (e.g. file-revert planning).
/// Never returns a summary, compacted item or truncated prefix as complete.
/// Callers performing mutations must separately lock/revalidate the binding.
pub fn read_complete_turn(
    key: &Key,
    thread_id: &str,
    turn_id: &str,
    call: &mut dyn FnMut(&str, Value) -> Result<Value, HistoryCallError>,
) -> Result<Option<CompleteHistoryTurn>, HistoryCallError> {
    read_turn(key, thread_id, turn_id, false, call)
        .map(|result| result.map(|(turn, _)| turn))
        .map_err(|error| match error {
            HistoryCallError::Rpc(native) if native["code"] == -32001 => {
                HistoryCallError::Rpc(json!({"code": -32001,
                    "message": "complete turn history unavailable",
                    "data": {"code": "history_turn_incomplete", "native": native}}))
            }
            error => error,
        })
}

/// Newer turns listed next to a complete read. Advisory only: their items are
/// `thread/turns/list itemsView=full` pages, which a native runtime may trim.
#[derive(Debug, Default)]
pub struct LaterTurns {
    /// Oldest first.
    pub turns: Vec<Value>,
    /// Newer turns past `later_limit` that were not listed.
    pub unscanned: usize,
}

/// `read_complete_turn` for one session, choosing the source like the HTTP
/// turn route: the live runtime while connected (falling back to the offline
/// reader only when it is unavailable), else the offline reader. Also lists up
/// to `later_limit` newer turns. `Ok(None)` = the turn is not in the thread.
pub fn read_complete_session_turn(
    cutex_session_id: &str,
    session: &Value,
    turn_id: &str,
    later_limit: usize,
    forward_native: ManagementNativeRequestHandler,
    open_offline: OfflineHistoryEndpointOpener,
) -> Result<Option<(CompleteHistoryTurn, LaterTurns)>, HistoryCallError> {
    let (result, source) = read_complete_session_turn_with(
        cutex_session_id,
        session,
        turn_id,
        later_limit,
        &mut |method, params| live_call(forward_native, cutex_session_id, method, params),
        &mut |method, params| offline_call(cutex_session_id, open_offline, method, params),
    )?;
    if source == "live" {
        evict_offline_reader(cutex_session_id);
    }
    Ok(result)
}

/// Browser lazy-image reads reuse the full single-turn reader, but MUST NOT
/// inherit its legacy whole-thread fallback. The guard sits immediately before
/// both native transports, including index construction and cached legacy keys.
pub fn read_complete_session_turn_paged(
    cutex_session_id: &str,
    session: &Value,
    turn_id: &str,
    forward_native: ManagementNativeRequestHandler,
    open_offline: OfflineHistoryEndpointOpener,
) -> Result<Option<CompleteHistoryTurn>, HistoryCallError> {
    let (result, source) = read_complete_session_turn_with(cutex_session_id, session, turn_id, 0,
        &mut |method, params| paged_history_call(method, params,
            &mut |method, params| live_call(forward_native, cutex_session_id, method, params)),
        &mut |method, params| paged_history_call(method, params,
            &mut |method, params| offline_call(cutex_session_id, open_offline, method, params)),
    )?;
    if source == "live" { evict_offline_reader(cutex_session_id); }
    Ok(result.map(|(turn, _)| turn))
}

fn paged_history_call(
    method: &str,
    params: Value,
    call: &mut dyn FnMut(&str, Value) -> Result<Value, HistoryCallError>,
) -> Result<Value, HistoryCallError> {
    let allowed = match method {
        "thread/read" => params["includeTurns"] == false,
        "thread/turns/list" => params["itemsView"] == "notLoaded",
        "thread/items/list" => true,
        _ => false,
    };
    if !allowed {
        return Err(HistoryCallError::Rpc(json!({"code":-32601,
            "message":"paged history is required for browser image reads",
            "data":{"code":"unsupported_operation"}})));
    }
    call(method, params)
}

#[allow(clippy::type_complexity)]
pub(super) fn read_complete_session_turn_with(
    cutex_session_id: &str,
    session: &Value,
    turn_id: &str,
    later_limit: usize,
    live: &mut dyn FnMut(&str, Value) -> Result<Value, HistoryCallError>,
    offline: &mut dyn FnMut(&str, Value) -> Result<Value, HistoryCallError>,
) -> Result<(Option<(CompleteHistoryTurn, LaterTurns)>, &'static str), HistoryCallError> {
    let thread_id = session
        .pointer("/native/threadId")
        .and_then(Value::as_str)
        .filter(|thread| !thread.is_empty())
        .ok_or_else(|| turn_index::invalid("the session has no bound native thread"))?;
    let read = |source: &str,
                call: &mut dyn FnMut(&str, Value) -> Result<Value, HistoryCallError>|
     -> Result<Option<(CompleteHistoryTurn, LaterTurns)>, HistoryCallError> {
        let key = index_key(cutex_session_id, session, source);
        let Some(turn) = read_complete_turn(&key, thread_id, turn_id, call)? else {
            return Ok(None);
        };
        let later = read_later_turns(thread_id, turn_id, &turn, later_limit, call)?;
        Ok(Some((turn, later)))
    };
    let connected = session
        .pointer("/runtime/appServerConnected")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if connected {
        match read("live", live) {
            Ok(result) => return Ok((result, "live")),
            // The runtime went away under us: read the rollout.
            Err(HistoryCallError::Unavailable(_)) => {}
            Err(error) => return Err(error),
        }
    }
    Ok((read("offline", offline)?, "offline"))
}

/// Newest-first pages down to `turn_id`, at most `limit` newer turns.
fn read_later_turns(
    thread_id: &str,
    turn_id: &str,
    target: &CompleteHistoryTurn,
    limit: usize,
    call: &mut dyn FnMut(&str, Value) -> Result<Value, HistoryCallError>,
) -> Result<LaterTurns, HistoryCallError> {
    const PAGE: usize = 50;
    let newer = target.turn_count.saturating_sub(target.ordinal);
    let wanted = newer.min(limit);
    let mut turns = Vec::new();
    let mut cursor = None;
    let mut cursors = HashSet::new();
    let mut reached = false;
    'pages: while turns.len() < wanted {
        let page = read_history_page(
            thread_id,
            &HistoryQuery {
                limit: (wanted - turns.len()).clamp(1, PAGE) as u32,
                cursor: cursor.clone(),
            },
            call,
        )?;
        let listed = page.thread["turns"].as_array().cloned().unwrap_or_default();
        for turn in listed.into_iter().rev() {
            if turn["id"].as_str() == Some(turn_id) {
                reached = true;
                break 'pages;
            }
            if turns.len() == wanted {
                break 'pages;
            }
            turns.push(turn);
        }
        cursor = page.next_cursor;
        let Some(next) = &cursor else {
            break;
        };
        if !cursors.insert(next.clone()) {
            return Err(turn_index::invalid("history cursor did not advance"));
        }
    }
    turns.reverse();
    Ok(LaterTurns {
        unscanned: if reached {
            0
        } else {
            newer.saturating_sub(turns.len())
        },
        turns,
    })
}

fn incomplete(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            map.get("truncated") == Some(&Value::Bool(true))
                || map
                    .get("itemsView")
                    .and_then(Value::as_str)
                    .is_some_and(|view| view != "full")
                || map.values().any(incomplete)
        }
        Value::Array(values) => values.iter().any(incomplete),
        _ => false,
    }
}

fn read_turn(
    key: &Key,
    thread_id: &str,
    turn_id: &str,
    preview: bool,
    call: &mut dyn FnMut(&str, Value) -> Result<Value, HistoryCallError>,
) -> Result<Option<(CompleteHistoryTurn, bool)>, HistoryCallError> {
    if !key.matches_thread(thread_id) {
        return Err(turn_index::invalid(
            "history index binding does not match requested thread",
        ));
    }
    let index = turn_index::read(key, call)?;
    let Some(ordinal) = index.ordinal(turn_id) else {
        return Ok(None);
    };
    let metadata = call(
        "thread/read",
        json!({"threadId": thread_id, "includeTurns": index.legacy}),
    )?;
    let mut thread = metadata
        .get("thread")
        .filter(|v| v.is_object())
        .cloned()
        .ok_or_else(|| turn_index::invalid("thread/read returned no thread"))?;
    if thread["id"].as_str() != Some(thread_id) {
        return Err(turn_index::invalid(
            "thread/read returned a different thread",
        ));
    }
    let mut turn = index.turns[ordinal - 1].clone();
    let budget = if preview {
        PREVIEW_BYTES
    } else {
        COMPLETE_BYTES
    };
    let mut items = Vec::new();
    let mut bytes = turn.to_string().len();
    let mut complete = true;
    if index.legacy {
        turn = thread["turns"]
            .as_array()
            .and_then(|turns| turns.iter().find(|t| t["id"] == turn_id))
            .cloned()
            .ok_or_else(|| turn_index::invalid("history changed during turn read"))?;
        complete = !incomplete(&turn);
        let mut metadata = turn.clone();
        metadata["items"] = json!([]);
        bytes = metadata.to_string().len();
        for item in turn["items"]
            .as_array()
            .ok_or_else(|| turn_index::invalid("history turn has no items"))?
        {
            bytes += item.to_string().len();
            if bytes > budget {
                complete = false;
                break;
            }
            items.push(item.clone());
        }
    } else {
        let mut cursor = None;
        let mut cursors = HashSet::new();
        let mut item_ids = HashSet::new();
        loop {
            let page = match call(
                "thread/items/list",
                json!({"threadId": thread_id, "turnId": turn_id,
                "sortDirection": "asc", "limit": 100, "cursor": cursor}),
            ) {
                Ok(page) => page,
                Err(HistoryCallError::Rpc(error)) if preview && error["code"] == -32001 => {
                    complete = false;
                    break;
                }
                Err(error) => return Err(error),
            };
            let entries = turn_index::page_data(&page)?;
            for entry in &entries {
                if entry["turnId"].as_str() != Some(turn_id) {
                    return Err(turn_index::invalid("items page returned a different turn"));
                }
                let item = entry
                    .get("item")
                    .filter(|v| v.is_object())
                    .ok_or_else(|| turn_index::invalid("history item missing"))?;
                let id = item["id"]
                    .as_str()
                    .ok_or_else(|| turn_index::invalid("history item has no id"))?;
                if !item_ids.insert(id.to_owned()) {
                    return Err(turn_index::invalid("duplicate history item id"));
                }
                bytes += item.to_string().len();
                if bytes > budget {
                    complete = false;
                    break;
                }
                complete &= !incomplete(item);
                items.push(item.clone());
            }
            if !complete {
                break;
            }
            cursor = turn_index::next_cursor(&page)?;
            let Some(next) = &cursor else {
                break;
            };
            if entries.is_empty() || !cursors.insert(next.clone()) || cursors.len() > 10_000 {
                return Err(turn_index::invalid(
                    "history item cursor did not advance or exceeded its capacity",
                ));
            }
        }
    }
    if !complete && !preview {
        return Err(HistoryCallError::Rpc(json!({"code": -32001,
            "message": "complete turn history unavailable: limit or native truncation",
            "data": {"code": "history_turn_incomplete"}})));
    }
    turn["items"] = json!(items);
    turn["itemsView"] = json!(if complete { "full" } else { "summary" });
    if !complete {
        turn["truncated"] = json!(true);
        turn["truncationReason"] = json!("history_preview_limit_or_native_truncation");
    }
    thread["turns"] = json!([turn]);
    Ok(Some((
        CompleteHistoryTurn {
            thread,
            ordinal,
            turn_count: index.turns.len(),
        },
        complete,
    )))
}

fn live_call(
    forward_native: ManagementNativeRequestHandler,
    cutex_session_id: &str,
    method: &str,
    params: Value,
) -> Result<Value, HistoryCallError> {
    let response = forward_native(
        cutex_session_id,
        json!({
            "id": format!("cutex-history-{}", uuid::Uuid::new_v4()),
            "method": method,
            "params": params,
        }),
    )
    .map_err(|error| {
        HistoryCallError::Unavailable(match error {
            ManagementNativeForwardError::BeforeForward(message)
            | ManagementNativeForwardError::NativeRequestIdInUse(message)
            | ManagementNativeForwardError::OutcomeUnknown(message) => message,
            ManagementNativeForwardError::StaleRuntimeGeneration { .. } => {
                "stale runtime generation".to_string()
            }
        })
    })?;
    if let Some(error) = response.get("error") {
        return Err(HistoryCallError::Rpc(error.clone()));
    }
    response
        .get("result")
        .cloned()
        .ok_or_else(|| HistoryCallError::Unavailable("native response has no result".into()))
}

/// Spawns readers through the bin-side opener for one session.
struct SessionReaderFactory {
    cutex_session_id: String,
    open: OfflineHistoryEndpointOpener,
}

impl CodexEndpointFactory for SessionReaderFactory {
    fn spawn(&self) -> Result<SpawnedEndpoint, CatalogError> {
        let (endpoint, codex_home) = (self.open)(&self.cutex_session_id)
            .map_err(|error| CatalogError::Launch(format!("{error:#}")))?;
        Ok(SpawnedEndpoint {
            endpoint,
            codex_home,
        })
    }
}

static READERS: OnceLock<Mutex<HashMap<String, Arc<CodexConfigHost>>>> = OnceLock::new();

fn readers() -> std::sync::MutexGuard<'static, HashMap<String, Arc<CodexConfigHost>>> {
    READERS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

/// Stop a session's warm offline reader, e.g. before its runtime starts.
pub fn evict_offline_reader(cutex_session_id: &str) {
    if let Some(host) = readers().remove(cutex_session_id) {
        host.shutdown();
    }
}

/// Structural edits may also invalidate a warm native reader's own snapshot.
pub fn invalidate_session(cutex_session_id: &str) {
    turn_index::invalidate(cutex_session_id);
    evict_offline_reader(cutex_session_id);
}

fn offline_call(
    cutex_session_id: &str,
    open: OfflineHistoryEndpointOpener,
    method: &str,
    params: Value,
) -> Result<Value, HistoryCallError> {
    let host = readers()
        .entry(cutex_session_id.to_string())
        .or_insert_with(|| {
            Arc::new(CodexConfigHost::new(
                Box::new(SessionReaderFactory {
                    cutex_session_id: cutex_session_id.to_string(),
                    open,
                }),
                READER_KEEP_WARM,
            ))
        })
        .clone();
    let (outcome, started) = host
        .call(method, params)
        .map_err(|error| HistoryCallError::Unavailable(error.to_string()))?;
    if let Some(generation) = started {
        super::codex_config::spawn_reaper(host, generation);
    }
    match outcome.result {
        Ok(value) => Ok(value),
        Err(CatalogError::Rpc { error, .. }) => Err(HistoryCallError::Rpc(json!({
            "code": error.code,
            "message": error.message,
            "data": error.data,
        }))),
        Err(error) => Err(HistoryCallError::Unavailable(error.to_string())),
    }
}

fn error(
    status: u16,
    reason: &'static str,
    code: &str,
    message: &str,
    retryable: bool,
    details: Value,
) -> HistoryResponse {
    HistoryResponse {
        status,
        reason,
        body: json!({
            "code": code,
            "message": message,
            "retryable": retryable,
            "details": details,
        }),
    }
}

/// Build the HTTP response for one session.
pub fn respond(
    cutex_session_id: &str,
    session: &Value,
    query: &HistoryQuery,
    forward_native: ManagementNativeRequestHandler,
    open_offline: OfflineHistoryEndpointOpener,
) -> HistoryResponse {
    let response = respond_with(
        cutex_session_id,
        session,
        query,
        &mut |method, params| live_call(forward_native, cutex_session_id, method, params),
        &mut |method, params| offline_call(cutex_session_id, open_offline, method, params),
    );
    if response.body.get("source").and_then(Value::as_str) == Some("live") {
        // The runtime owns the home again; never keep a second process on it.
        evict_offline_reader(cutex_session_id);
    }
    response
}

fn respond_with(
    cutex_session_id: &str,
    session: &Value,
    query: &HistoryQuery,
    live: &mut dyn FnMut(&str, Value) -> Result<Value, HistoryCallError>,
    offline: &mut dyn FnMut(&str, Value) -> Result<Value, HistoryCallError>,
) -> HistoryResponse {
    respond_read_with(cutex_session_id, session, Some(query), None, live, offline)
}

pub fn respond_turn(
    cutex_session_id: &str,
    session: &Value,
    turn_id: &str,
    forward_native: ManagementNativeRequestHandler,
    open_offline: OfflineHistoryEndpointOpener,
) -> HistoryResponse {
    let response = respond_read_with(
        cutex_session_id,
        session,
        None,
        Some(turn_id),
        &mut |method, params| live_call(forward_native, cutex_session_id, method, params),
        &mut |method, params| offline_call(cutex_session_id, open_offline, method, params),
    );
    if response.body.get("source").and_then(Value::as_str) == Some("live") {
        evict_offline_reader(cutex_session_id);
    }
    response
}

fn respond_read_with(
    cutex_session_id: &str,
    session: &Value,
    query: Option<&HistoryQuery>,
    turn_id: Option<&str>,
    live: &mut dyn FnMut(&str, Value) -> Result<Value, HistoryCallError>,
    offline: &mut dyn FnMut(&str, Value) -> Result<Value, HistoryCallError>,
) -> HistoryResponse {
    // Claude sessions take the same paths: the live adapter answers while
    // connected, the offline reader is the adapter's read-only history reader.
    let Some(thread_id) = session
        .pointer("/native/threadId")
        .and_then(Value::as_str)
        .filter(|thread_id| !thread_id.is_empty())
    else {
        return error(
            409,
            "Conflict",
            "thread_unbound",
            "the cutex session has no bound native thread yet",
            false,
            json!({ "cutexSessionId": cutex_session_id }),
        );
    };
    let connected = session
        .pointer("/runtime/appServerConnected")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut result = None;
    let read = |source: &str,
                call: &mut dyn FnMut(&str, Value) -> Result<Value, HistoryCallError>|
     -> Result<Option<Value>, HistoryCallError> {
        let key = index_key(cutex_session_id, session, source);
        let mut body = json!({"contractVersion": 2, "cutexSessionId": cutex_session_id, "threadId": thread_id, "source": source});
        if let Some(turn_id) = turn_id {
            let Some((turn, complete)) = read_turn(&key, thread_id, turn_id, true, call)? else {
                return Ok(None);
            };
            body["turnId"] = json!(turn_id);
            body["ordinal"] = json!(turn.ordinal);
            body["turnCount"] = json!(turn.turn_count);
            body["complete"] = json!(complete);
            body["native"] = json!({"method": "thread/read", "result": {"thread": turn.thread}});
        } else {
            let query = query.expect("page query");
            let page = read_indexed_page(&key, thread_id, query, call)?;
            body["page"] = page_json(query.limit, query.cursor.as_deref(), &page);
            body["native"] = json!({"method": "thread/read", "result": {"thread": page.thread}});
        }
        Ok(Some(body))
    };
    if connected {
        match read("live", live) {
            Ok(page) => result = Some(("live", page)),
            // The runtime went away under us: answer from the rollout.
            Err(HistoryCallError::Unavailable(_)) => {}
            Err(HistoryCallError::Rpc(native)) => return native_error(thread_id, native),
        }
    }
    let (_source, body) = match result {
        Some(result) => result,
        None => match read("offline", offline) {
            Ok(page) => ("offline", page),
            Err(HistoryCallError::Rpc(native)) => return native_error(thread_id, native),
            Err(HistoryCallError::Unavailable(diagnostic)) => {
                return error(
                    503,
                    "Service Unavailable",
                    "codex_app_server_unavailable",
                    "the native history reader could not be started",
                    true,
                    json!({ "diagnostic": diagnostic }),
                )
            }
        },
    };
    let Some(body) = body else {
        return error(
            404,
            "Not Found",
            "turn_not_found",
            "the turn does not exist in the current thread",
            false,
            json!({"cutexSessionId": cutex_session_id, "threadId": thread_id, "turnId": turn_id}),
        );
    };
    HistoryResponse {
        status: 200,
        reason: "OK",
        body,
    }
}

/// Adds `cutexState.{latestPlan, latestTokenUsage}` to a success body.
pub fn attach_cutex_state(body: &mut Value, latest_plan: Value, latest_token_usage: Value) {
    if let Some(body) = body.as_object_mut() {
        body.insert(
            "cutexState".to_string(),
            json!({ "latestPlan": latest_plan, "latestTokenUsage": latest_token_usage }),
        );
    }
}

fn native_error(thread_id: &str, native: Value) -> HistoryResponse {
    error(
        502,
        "Bad Gateway",
        "native_history_unavailable",
        "the native app-server could not read this thread's history",
        false,
        json!({ "threadId": thread_id, "native": native }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_history_paged_reader_never_dispatches_legacy_whole_thread_reads() {
        for connected in [false, true] {
            let mut called = Vec::new();
            let mut old = |method: &str, params: Value| {
                called.push((method.to_owned(), params));
                Err(HistoryCallError::Rpc(json!({"code":-32601,"message":"unknown method"})))
            };
            let mut guarded = |method: &str, params: Value| paged_history_call(method, params, &mut old);
            let mut unused = |_: &str, _: Value| panic!("unsupported history must not change runtime source");
            let result = if connected {
                read_complete_session_turn_with(&uuid::Uuid::new_v4().to_string(), &session(true), "turn-0", 0, &mut guarded, &mut unused)
            } else {
                read_complete_session_turn_with(&uuid::Uuid::new_v4().to_string(), &session(false), "turn-0", 0, &mut unused, &mut guarded)
            };
            assert!(matches!(result, Err(HistoryCallError::Rpc(ref error)) if error.pointer("/data/code") == Some(&json!("unsupported_operation"))));
            assert!(called.iter().all(|(method, _)| method != "thread/read"));
        }
    }

    #[test]
    fn browser_history_paged_reader_retains_exact_turn_and_item_paging() {
        let mut fake = native(3, true);
        let mut calls = Vec::new();
        let mut guarded = |method: &str, params: Value| paged_history_call(method, params, &mut |method, params| {
            calls.push((method.to_owned(), params.clone())); fake(method, params)
        });
        let result = read_complete_session_turn_with(&uuid::Uuid::new_v4().to_string(), &session(true), "turn-1", 0,
            &mut guarded, &mut |_, _| panic!("live reader is available")).unwrap().0.unwrap().0;
        assert_eq!(result.turn()["id"], "turn-1");
        assert!(calls.iter().any(|(method, params)| method == "thread/items/list" && params["turnId"] == "turn-1"));
        assert!(calls.iter().all(|(method, params)| method != "thread/read" || params["includeTurns"] == false));
    }

    fn turn(id: usize) -> Value {
        json!({ "id": format!("turn-{id}"), "status": "completed", "items": [], "startedAt": id, "completedAt": id })
    }

    fn session(connected: bool) -> Value {
        json!({
            "native": { "threadId": "thread-1" },
            "runtime": { "backend": "host", "appServerConnected": connected },
        })
    }

    /// Native fake: `turns` total turns, newest last, paging like thread/turns/list.
    fn native(
        turns: usize,
        supports_list: bool,
    ) -> impl FnMut(&str, Value) -> Result<Value, HistoryCallError> {
        move |method, params| match method {
            "thread/read" => {
                let include = params["includeTurns"].as_bool().unwrap();
                let all: Vec<Value> = if include {
                    (0..turns).map(turn).collect()
                } else {
                    Vec::new()
                };
                Ok(json!({ "thread": { "id": "thread-1", "preview": "p", "turns": all } }))
            }
            "thread/turns/list" if supports_list => {
                assert_eq!(params["sortDirection"], "desc");
                assert!(["full", "notLoaded"].contains(&params["itemsView"].as_str().unwrap()));
                let limit = params["limit"].as_u64().unwrap() as usize;
                let skip: usize = params["cursor"]
                    .as_str()
                    .map(|c| c.parse().unwrap())
                    .unwrap_or(0);
                let data: Vec<Value> = (0..turns).rev().skip(skip).take(limit).map(turn).collect();
                let next = (skip + data.len() < turns).then(|| (skip + data.len()).to_string());
                Ok(json!({ "data": data, "nextCursor": next, "backwardsCursor": null }))
            }
            "thread/items/list" if supports_list => Ok(json!({"data": [], "nextCursor": null})),
            _ => Err(HistoryCallError::Rpc(
                json!({ "code": -32600, "message": "unknown method" }),
            )),
        }
    }

    fn ids(page: &HistoryPage) -> Vec<String> {
        page.thread["turns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|turn| turn["id"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn query_defaults_and_bounds() {
        let mut request = SimpleHttpRequest {
            method: "GET".into(),
            path: "/v2/sessions/s/history".into(),
            headers: Default::default(),
            body: Vec::new(),
        };
        assert_eq!(
            parse_query(&request).unwrap(),
            HistoryQuery {
                limit: DEFAULT_LIMIT,
                cursor: None
            }
        );
        request.path = "/v2/sessions/s/history?limit=5&cursor=abc".into();
        assert_eq!(
            parse_query(&request).unwrap(),
            HistoryQuery {
                limit: 5,
                cursor: Some("abc".into())
            }
        );
        for bad in ["limit=0", "limit=201", "limit=x", "other=1"] {
            request.path = format!("/v2/sessions/s/history?{bad}");
            assert!(parse_query(&request).is_err(), "{bad}");
        }
    }

    #[test]
    fn bootstrap_turn_limit_is_optional_and_bounded() {
        let mut request = SimpleHttpRequest {
            method: "GET".into(),
            path: "/v2/sessions/s/bootstrap".into(),
            headers: Default::default(),
            body: Vec::new(),
        };
        assert_eq!(parse_bootstrap_turn_limit(&request).unwrap(), None);
        request.path = "/v2/sessions/s/bootstrap?turnLimit=20&other=x".into();
        assert_eq!(parse_bootstrap_turn_limit(&request).unwrap(), Some(20));
        for bad in ["turnLimit=0", "turnLimit=201", "turnLimit=x"] {
            request.path = format!("/v2/sessions/s/bootstrap?{bad}");
            assert!(parse_bootstrap_turn_limit(&request).is_err(), "{bad}");
        }
    }

    #[test]
    fn rpc_call_maps_native_errors_and_transport_failures() {
        let mut ok =
            |request: Value| Ok(json!({ "id": request["id"].clone(), "result": { "x": 1 } }));
        assert_eq!(
            rpc_call(&mut ok)("m", json!({})).unwrap(),
            json!({ "x": 1 })
        );
        let mut rpc = |_: Value| Ok(json!({ "id": "1", "error": { "code": -32601 } }));
        assert_eq!(
            rpc_call(&mut rpc)("m", json!({})),
            Err(HistoryCallError::Rpc(json!({ "code": -32601 })))
        );
        let mut down = |_: Value| Err("gone".to_string());
        assert_eq!(
            rpc_call(&mut down)("m", json!({})),
            Err(HistoryCallError::Unavailable("gone".into()))
        );
    }

    #[test]
    fn pages_newest_turns_oldest_first_with_native_cursor() {
        let mut call = native(5, true);
        let first = read_history_page(
            "thread-1",
            &HistoryQuery {
                limit: 2,
                cursor: None,
            },
            &mut call,
        )
        .unwrap();
        assert_eq!(ids(&first), ["turn-3", "turn-4"]);
        assert_eq!(first.thread["preview"], "p");
        let second = read_history_page(
            "thread-1",
            &HistoryQuery {
                limit: 2,
                cursor: first.next_cursor.clone(),
            },
            &mut call,
        )
        .unwrap();
        assert_eq!(ids(&second), ["turn-1", "turn-2"]);
        let third = read_history_page(
            "thread-1",
            &HistoryQuery {
                limit: 2,
                cursor: second.next_cursor.clone(),
            },
            &mut call,
        )
        .unwrap();
        assert_eq!(ids(&third), ["turn-0"]);
        assert_eq!(third.next_cursor, None);
    }

    #[test]
    fn falls_back_to_full_thread_read_when_turn_listing_is_unknown() {
        let mut call = native(5, false);
        let first = read_history_page(
            "thread-1",
            &HistoryQuery {
                limit: 2,
                cursor: None,
            },
            &mut call,
        )
        .unwrap();
        assert_eq!(ids(&first), ["turn-3", "turn-4"]);
        assert_eq!(first.next_cursor.as_deref(), Some("cutex-offset:2"));
        let second = read_history_page(
            "thread-1",
            &HistoryQuery {
                limit: 3,
                cursor: first.next_cursor.clone(),
            },
            &mut call,
        )
        .unwrap();
        assert_eq!(ids(&second), ["turn-0", "turn-1", "turn-2"]);
        assert_eq!(second.next_cursor, None);
    }

    #[test]
    fn offline_session_reads_the_rollout_without_the_live_runtime() {
        let mut live = |_: &str, _: Value| -> Result<Value, HistoryCallError> {
            panic!("offline sessions must not touch the live runtime")
        };
        let mut reader = native(3, true);
        let mut offline = |method: &str, params: Value| reader(method, params);
        let response = respond_with(
            "s",
            &session(false),
            &HistoryQuery {
                limit: 20,
                cursor: None,
            },
            &mut live,
            &mut offline,
        );
        assert_eq!(response.status, 200);
        assert_eq!(response.body["source"], "offline");
        assert_eq!(response.body["native"]["method"], "thread/read");
        assert_eq!(
            response.body["native"]["result"]["thread"]["turns"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        assert_eq!(response.body["page"]["hasMore"], false);

        let mut body = response.body.clone();
        attach_cutex_state(&mut body, Value::Null, Value::Null);
        assert_eq!(
            body["cutexState"],
            json!({ "latestPlan": null, "latestTokenUsage": null })
        );
        let plan = json!({ "threadId": "thread-1", "turnId": "turn-2", "explanation": "e",
                           "plan": [{ "step": "a", "status": "completed" }],
                           "updatedAt": "2026-10-07T00:00:00.000Z" });
        let usage = json!({ "threadId": "thread-1", "turnId": "turn-2",
                            "tokenUsage": { "total": {}, "last": {}, "modelContextWindow": null },
                            "updatedAt": "2026-10-07T00:00:00.000Z" });
        attach_cutex_state(&mut body, plan.clone(), usage.clone());
        assert_eq!(body["cutexState"]["latestPlan"], plan);
        assert_eq!(body["cutexState"]["latestTokenUsage"], usage);
        assert_eq!(body["source"], "offline");
    }

    #[test]
    fn live_session_proxies_and_falls_back_when_the_runtime_vanishes() {
        let mut live = native(2, true);
        let mut offline = |_: &str, _: Value| -> Result<Value, HistoryCallError> {
            panic!("a live runtime answers directly")
        };
        let response = respond_with(
            "s",
            &session(true),
            &HistoryQuery {
                limit: 20,
                cursor: None,
            },
            &mut live,
            &mut offline,
        );
        assert_eq!(response.body["source"], "live");

        let mut gone = |_: &str, _: Value| Err(HistoryCallError::Unavailable("gone".into()));
        let mut reader = native(1, true);
        let mut offline = |method: &str, params: Value| reader(method, params);
        let response = respond_with(
            "s",
            &session(true),
            &HistoryQuery {
                limit: 20,
                cursor: None,
            },
            &mut gone,
            &mut offline,
        );
        assert_eq!(response.body["source"], "offline");
    }

    #[test]
    fn unbound_sessions_are_typed_conflicts() {
        let mut never = |_: &str, _: Value| -> Result<Value, HistoryCallError> { panic!() };
        let mut never_offline = |_: &str, _: Value| -> Result<Value, HistoryCallError> { panic!() };
        let unbound = json!({ "native": { "threadId": null }, "runtime": { "backend": "host" } });
        let response = respond_with(
            "s",
            &unbound,
            &HistoryQuery {
                limit: 1,
                cursor: None,
            },
            &mut never,
            &mut never_offline,
        );
        assert_eq!(response.body["code"], "thread_unbound");
    }

    #[test]
    fn claude_sessions_read_live_or_from_the_offline_reader() {
        let mut claude = session(true);
        claude["runtime"]["backend"] = json!("claude_subscription");
        let mut live = native(2, true);
        let mut never = |_: &str, _: Value| -> Result<Value, HistoryCallError> { panic!() };
        let response = respond_with(
            "s",
            &claude,
            &HistoryQuery {
                limit: 20,
                cursor: None,
            },
            &mut live,
            &mut never,
        );
        assert_eq!(response.body["source"], "live");
        claude["runtime"]["appServerConnected"] = json!(false);
        let mut never = |_: &str, _: Value| -> Result<Value, HistoryCallError> { panic!() };
        let mut reader = native(3, true);
        let mut offline = |method: &str, params: Value| reader(method, params);
        let response = respond_with(
            "s",
            &claude,
            &HistoryQuery {
                limit: 2,
                cursor: None,
            },
            &mut never,
            &mut offline,
        );
        assert_eq!(response.body["source"], "offline");
        assert_eq!(response.body["page"]["hasMore"], true);
    }

    fn unique_key() -> Key {
        Key::new(&uuid::Uuid::new_v4().to_string(), "thread-1", "live", 0)
    }

    #[test]
    fn turn_read_live_offline_claude_fallback_not_found_and_unbound() {
        for backend in ["host", "claude_subscription"] {
            for connected in [false, true] {
                let mut session = session(connected);
                session["runtime"]["backend"] = json!(backend);
                for id in ["turn-0", "turn-2", "turn-5", "absent"] {
                    let response = respond_read_with(
                        &uuid::Uuid::new_v4().to_string(),
                        &session,
                        None,
                        Some(id),
                        &mut native(6, true),
                        &mut native(6, true),
                    );
                    if id == "absent" {
                        assert_eq!(response.status, 404);
                        assert_eq!(response.body["code"], "turn_not_found");
                        assert_eq!(response.body["details"]["turnId"], id);
                    } else {
                        assert_eq!(response.status, 200);
                        assert_eq!(
                            response.body["source"],
                            if connected { "live" } else { "offline" }
                        );
                        assert_eq!(response.body["turnCount"], 6);
                        assert_eq!(
                            response.body["ordinal"],
                            id[5..].parse::<usize>().unwrap() + 1
                        );
                        assert_eq!(
                            response.body["native"]["result"]["thread"]["turns"]
                                .as_array()
                                .unwrap()
                                .len(),
                            1
                        );
                        assert_eq!(response.body["complete"], true);
                        assert!(response.body.get("cutexState").is_none());
                        super::super::schema_export::assert_valid(
                            "bootstrap",
                            "historyTurn",
                            &response.body,
                        );
                    }
                }
            }
        }
        let response = respond_read_with(
            "turn-fallback",
            &session(true),
            None,
            Some("turn-1"),
            &mut |_, _| Err(HistoryCallError::Unavailable("gone".into())),
            &mut native(2, true),
        );
        assert_eq!(response.body["source"], "offline");
        let response = respond_read_with(
            "unbound",
            &json!({}),
            None,
            Some("turn"),
            &mut native(0, true),
            &mut native(0, true),
        );
        assert_eq!(response.status, 409);
    }

    fn item_native(
        count: usize,
        item_bytes: usize,
    ) -> impl FnMut(&str, Value) -> Result<Value, HistoryCallError> {
        let mut base = native(1, true);
        move |method, params| {
            if method != "thread/items/list" {
                return base(method, params);
            }
            assert_eq!(params["turnId"], "turn-0");
            assert_eq!(params["sortDirection"], "asc");
            let offset = params["cursor"]
                .as_str()
                .map(|s| s.parse::<usize>().unwrap())
                .unwrap_or(0);
            let end = (offset + 2).min(count);
            let data: Vec<_> = (offset..end).map(|n| json!({"turnId":"turn-0", "item": {
                "id":format!("item-{n}"), "type":"fileChange", "changes":[{"path":"fixture.txt", "diff":"x".repeat(item_bytes)}]
            }})).collect();
            Ok(json!({"data":data, "nextCursor":(end < count).then(|| end.to_string())}))
        }
    }

    #[test]
    fn large_turn_pages_items_preview_limits_and_complete_read_fails_closed() {
        let key = unique_key();
        let (turn, complete) = read_turn(
            &key,
            "thread-1",
            "turn-0",
            true,
            &mut item_native(9, 80_000),
        )
        .unwrap()
        .unwrap();
        assert!(!complete);
        assert_eq!(turn.turn()["itemsView"], "summary");
        assert_eq!(turn.turn()["truncated"], true);
        assert!(turn.turn().to_string().len() < PREVIEW_BYTES);
        let complete = read_complete_turn(&key, "thread-1", "turn-0", &mut item_native(9, 80_000))
            .unwrap()
            .unwrap();
        assert_eq!(complete.turn()["items"].as_array().unwrap().len(), 9);
        assert_eq!(complete.turn()["itemsView"], "full");
        let error = read_complete_turn(
            &key,
            "thread-1",
            "turn-0",
            &mut item_native(2, COMPLETE_BYTES),
        )
        .unwrap_err();
        assert!(matches!(error, HistoryCallError::Rpc(ref value)
            if value["code"] == -32001 && value["data"]["code"] == "history_turn_incomplete"));
        let mut base = item_native(1, 20);
        let mut truncated = |method: &str, params: Value| {
            let mut page = base(method, params)?;
            if method == "thread/items/list" {
                page["data"][0]["item"]["truncated"] = json!(true);
            }
            Ok(page)
        };
        assert!(read_complete_turn(&key, "thread-1", "turn-0", &mut truncated).is_err());
    }

    #[test]
    fn item_cursor_duplicate_and_foreign_turn_are_errors() {
        for mode in 0..5 {
            let mut base = native(1, true);
            let mut page_n = 0;
            let result = read_complete_turn(
                &unique_key(),
                "thread-1",
                "turn-0",
                &mut |method, params| {
                    if method != "thread/items/list" {
                        return base(method, params);
                    }
                    page_n += 1;
                    let mut page = json!({"data":[{"turnId":if mode == 2 {"foreign"} else {"turn-0"},
                    "item":{"id":if mode == 0 {"duplicate".to_string()} else {format!("i{page_n}")}}}],
                    "nextCursor":"stuck"});
                    if mode == 3 {
                        page.as_object_mut().unwrap().remove("nextCursor");
                    }
                    if mode == 4 {
                        page["nextCursor"] = json!(42);
                    }
                    Ok(page)
                },
            );
            assert!(result.is_err());
            assert!(page_n <= 2);
        }
    }

    #[test]
    fn page_ordinals_first_middle_last_empty_and_reverted_turn() {
        let key = unique_key();
        let mut native = native(5, true);
        let query = |cursor: Option<String>| HistoryQuery { limit: 2, cursor };
        // Cold: the newest page alone cannot number a longer thread and the
        // page read adds no native calls for an index.
        let mut calls = 0;
        let page = read_indexed_page(&key, "thread-1", &query(None), &mut |method, params| {
            calls += 1;
            native(method, params)
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!((page.first_ordinal, page.turn_count), (None, None));
        let mut cursor = page.next_cursor;
        let page =
            read_indexed_page(&key, "thread-1", &query(cursor.clone()), &mut native).unwrap();
        assert_eq!((page.first_ordinal, page.turn_count), (None, None));
        // A single-turn read builds the index; pages use it from then on.
        read_complete_turn(&key, "thread-1", "turn-1", &mut native)
            .unwrap()
            .unwrap();
        cursor = None;
        for (first, size, count) in [(4, 2, Some(5)), (2, 2, None), (1, 1, None)] {
            let page =
                read_indexed_page(&key, "thread-1", &query(cursor.clone()), &mut native).unwrap();
            assert_eq!(page.first_ordinal, Some(first));
            assert_eq!(page.turn_count, count);
            super::super::schema_export::assert_valid(
                "bootstrap",
                "historyPage",
                &page_json(2, cursor.as_deref(), &page),
            );
            for (i, t) in page.thread["turns"].as_array().unwrap().iter().enumerate() {
                assert_eq!(t["id"], format!("turn-{}", first + i - 1));
            }
            assert_eq!(page.thread["turns"].as_array().unwrap().len(), size);
            cursor = page.next_cursor;
        }
        let page =
            read_indexed_page(&key, "thread-1", &query(Some("5".into())), &mut native).unwrap();
        assert_eq!((page.first_ordinal, page.turn_count), (None, None));
        // Appended turns: the newest page reconciles with the cached index.
        let mut grown = self::native(6, true);
        let page = read_indexed_page(&key, "thread-1", &query(None), &mut grown).unwrap();
        assert_eq!((page.first_ordinal, page.turn_count), (Some(5), Some(6)));
        // A thread that fits in the newest page is numbered cold.
        let short = unique_key();
        let page = read_indexed_page(
            &short,
            "thread-1",
            &HistoryQuery {
                limit: 10,
                cursor: None,
            },
            &mut self::native(3, true),
        )
        .unwrap();
        assert_eq!((page.first_ordinal, page.turn_count), (Some(1), Some(3)));
        assert!(
            read_complete_turn(&key, "thread-1", "turn-4", &mut self::native(4, true))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn single_turn_old_native_and_compact_query() {
        let turn = read_complete_turn(&unique_key(), "thread-1", "turn-2", &mut native(5, false))
            .unwrap()
            .unwrap();
        assert_eq!(turn.ordinal, 3);
        assert_eq!(turn.turn_count, 5);
        let mut body = respond_read_with(
            "compact-turn",
            &session(true),
            None,
            Some("turn-0"),
            &mut item_native(1, 5000),
            &mut native(0, true),
        )
        .body;
        assert_eq!(super::super::compact::compact_items(&mut body), 1);
        assert_eq!(
            body["native"]["result"]["thread"]["turns"][0]["items"][0]["truncated"],
            true
        );
        for (query, valid) in [("compact=1", true), ("limit=1", false), ("cursor=x", false)] {
            let request = SimpleHttpRequest {
                method: "GET".into(),
                path: format!("/v2/sessions/s/history/turns/t?{query}"),
                headers: Default::default(),
                body: vec![],
            };
            assert_eq!(parse_turn_query(&request).is_ok(), valid);
        }
    }

    #[test]
    fn synthetic_1500_turn_cold_hot_and_bootstrap_timings() {
        let key = unique_key();
        let mut calls = 0;
        let mut base = native(1500, true);
        let mut call = |method: &str, params: Value| {
            calls += 1;
            std::thread::sleep(Duration::from_millis(1));
            base(method, params)
        };
        let query = HistoryQuery {
            limit: 20,
            cursor: None,
        };
        let start = std::time::Instant::now();
        let mut page = read_history_page("thread-1", &query, &mut call).unwrap();
        let before = start.elapsed();
        let start = std::time::Instant::now();
        annotate_bootstrap(&key, &mut page);
        let annotation = start.elapsed();
        assert_eq!(page.turn_count, None);
        let start = std::time::Instant::now();
        let cold_page = read_indexed_page(&key, "thread-1", &query, &mut call).unwrap();
        let cold = start.elapsed();
        assert_eq!(cold_page.turn_count, None);
        let start = std::time::Instant::now();
        read_complete_turn(&key, "thread-1", "turn-1499", &mut call)
            .unwrap()
            .unwrap();
        let index_build = start.elapsed();
        let start = std::time::Instant::now();
        let hot = read_indexed_page(&key, "thread-1", &query, &mut call).unwrap();
        let hot_time = start.elapsed();
        assert_eq!(hot.first_ordinal, Some(1481));
        assert_eq!(hot.turn_count, Some(1500));
        eprintln!("TURN_READ_BENCH synthetic=1500 rpc_delay_ms=1 bootstrap_before_us={} bootstrap_after_us={} cold_page_us={} turn_read_index_build_us={} hot_us={} calls={calls}",
            before.as_micros(), (before + annotation).as_micros(), cold.as_micros(), index_build.as_micros(), hot_time.as_micros());
    }
}
