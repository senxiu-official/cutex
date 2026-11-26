//! Opt-in bootstrap bus windows and owner-only keyset history. Selection is
//! read-only and happens before serializing frozen runtime receipts.
use super::*;
use crate::http::server::SimpleHttpRequest;
use base64::Engine;
use std::collections::{BTreeSet, HashMap};

pub const CAPABILITY: &str = "bootstrap-bounded-bus";
pub const UNLINKED_LIMIT: usize = 20;
const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 200;

#[derive(Debug, Default)]
pub struct Window {
    turns: BTreeSet<String>,
    items: BTreeSet<String>,
}

impl Window {
    pub fn from_native(native: &Value) -> Self {
        let mut window = Self::default();
        for turn in native
            .pointer("/result/thread/turns")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(id) = turn["id"].as_str() {
                window.turns.insert(id.into());
            }
            for item in turn["items"].as_array().into_iter().flatten() {
                if let Some(id) = item["id"].as_str() {
                    window.items.insert(id.into());
                }
            }
        }
        window
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Direction {
    Received,
    Sent,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct Key(i64, String);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    version: u8,
    session: String,
    direction: Direction,
    before: Key,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    filter: Option<String>,
}

pub struct Query {
    pub direction: Direction,
    pub limit: usize,
    before: Option<Key>,
    window: Option<Window>,
}

fn filter_key(window: Option<&Window>) -> Option<String> {
    use sha2::{Digest, Sha256};
    window.map(|window| {
        format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(&window.turns, &window.items)).expect("string sets serialize")
            )
        )
    })
}

fn parse_filter(params: &HashMap<String, String>) -> Result<Option<Window>, String> {
    if !params.contains_key("turnIds") && !params.contains_key("itemIds") {
        return Ok(None);
    }
    let mut window = Window::default();
    if params.get("turnIds").map_or(0, String::len) + params.get("itemIds").map_or(0, String::len)
        > 16 * 1024
    {
        return Err("bus history filter is too long".into());
    }
    for (name, ids) in [
        ("turnIds", &mut window.turns),
        ("itemIds", &mut window.items),
    ] {
        if let Some(value) = params.get(name) {
            let parts: Vec<_> = value.split(',').collect();
            if parts.len() > 200
                || parts
                    .iter()
                    .any(|id| id.is_empty() || id.len() > 512 || id.chars().any(char::is_control))
            {
                return Err(
                    "turnIds/itemIds must contain 1..200 nonempty IDs of at most 512 bytes".into(),
                );
            }
            ids.extend(parts.into_iter().map(str::to_string));
        }
    }
    Ok(Some(window))
}

fn params(request: &SimpleHttpRequest) -> Result<HashMap<String, String>, String> {
    let mut params = HashMap::new();
    let query = request.path.split_once('?').map_or("", |(_, query)| query);
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if matches!(
            key.as_ref(),
            "bus" | "direction" | "limit" | "cursor" | "turnIds" | "itemIds"
        ) && params
            .insert(key.into_owned(), value.into_owned())
            .is_some()
        {
            return Err("duplicate bus history query parameter".into());
        }
    }
    Ok(params)
}

pub fn opted_in(request: &SimpleHttpRequest) -> Result<bool, String> {
    // Other bootstrap query parameters keep their existing parsers/semantics.
    let query = request.path.split_once('?').map_or("", |(_, query)| query);
    let mut values = url::form_urlencoded::parse(query.as_bytes()).filter(|(key, _)| key == "bus");
    let bus = values.next().map(|(_, value)| value.into_owned());
    if values.next().is_some() {
        return Err("duplicate bus query parameter".into());
    }
    match bus.as_deref() {
        None | Some("full") => Ok(false),
        Some("bounded") => Ok(true),
        _ => Err("bus must be full or bounded".into()),
    }
}

pub fn parse_query(session: &str, request: &SimpleHttpRequest) -> Result<Query, String> {
    let params = params(request)?;
    let direction = match params.get("direction").map(String::as_str) {
        None | Some("received") => Direction::Received,
        Some("sent") => Direction::Sent,
        _ => return Err("direction must be received or sent".into()),
    };
    let limit = match params.get("limit") {
        None => DEFAULT_LIMIT,
        Some(value) => value
            .parse::<usize>()
            .ok()
            .filter(|limit| (1..=MAX_LIMIT).contains(limit))
            .ok_or("limit must be 1..200")?,
    };
    let window = parse_filter(&params)?;
    let before = match params.get("cursor") {
        None => None,
        Some(value) => {
            if value.is_empty() || value.len() > 2048 {
                return Err("invalid bus history cursor".into());
            }
            let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(value)
                .map_err(|_| "invalid bus history cursor")?;
            let cursor: Cursor =
                serde_json::from_slice(&bytes).map_err(|_| "invalid bus history cursor")?;
            if cursor.version != 1
                || cursor.session != session
                || cursor.direction != direction
                || cursor.filter != filter_key(window.as_ref())
                || cursor.before.1.is_empty()
                || cursor.before.1.len() > 512
            {
                return Err("bus history cursor scope or version mismatch".into());
            }
            Some(cursor.before)
        }
    };
    Ok(Query {
        direction,
        limit,
        before,
        window,
    })
}

pub fn parse_path(path: &str) -> Option<String> {
    let id = path
        .strip_prefix("/v2/sessions/")?
        .strip_suffix("/bus-messages")?;
    crate::management::v2::server::decode_path_segment(id)
}

fn key(row: &StoredAgentBusMessage) -> Key {
    // Never use updated_at: delivery/settlement changes must not move a row
    // across an already-issued page cursor. Ancient undated rows sort by id.
    let time = row
        .canonical_envelope
        .as_ref()
        .map(|e| e.created_at_epoch_secs)
        .filter(|time| *time > 0)
        .and_then(|time| i64::try_from(time).ok())
        .map(|time| time.saturating_mul(1000))
        .or_else(|| {
            row.queued_at
                .as_deref()
                .and_then(|time| DateTime::parse_from_rfc3339(time).ok())
                .map(|time| time.timestamp_millis())
        })
        .unwrap_or(0);
    Key(time, row.snapshot.message_id.clone())
}

fn belongs(row: &StoredAgentBusMessage, session: &str, direction: Direction) -> bool {
    match direction {
        Direction::Received => row.owner_cutex_session_id == session,
        Direction::Sent => row.snapshot.from_cutex_session_id.as_deref() == Some(session),
    }
}

fn turn(row: &StoredAgentBusMessage, direction: Direction) -> Option<&str> {
    let meta = row.meta.as_ref();
    match direction {
        Direction::Received => meta
            .and_then(|meta| meta.context_turn_id.as_deref())
            .or_else(|| {
                row.snapshot
                    .a4_receipt
                    .as_ref()
                    .map(|receipt| receipt.turn_id.as_str())
            }),
        Direction::Sent => meta.and_then(|meta| meta.facts.sender_turn_id.as_deref()),
    }
    .filter(|id| !id.is_empty())
}

fn required(row: &StoredAgentBusMessage) -> bool {
    !matches!(
        row.snapshot.state.as_str(),
        "delivered" | "discarded" | "failed" | "followup_closed"
    ) || settlement_view(row, Utc::now()).is_some_and(|view| view.state == "outcome_unknown")
}

fn matches_window(row: &StoredAgentBusMessage, direction: Direction, window: &Window) -> bool {
    turn(row, direction).is_some_and(|turn| window.turns.contains(turn))
        || window.items.contains(&row.snapshot.message_id)
}

fn row_value(row: &StoredAgentBusMessage, direction: Direction) -> anyhow::Result<Value> {
    match direction {
        Direction::Received => received_row(row),
        Direction::Sent => sent_row(row),
    }
}

fn sent_row(row: &StoredAgentBusMessage) -> anyhow::Result<Value> {
    let fields = stored_agent_fields(row);
    let mut value = json!({
        "messageId": row.snapshot.message_id, "toCutexSessionId": row.snapshot.to_cutex_session_id,
        "recipient": fields.get("recipient"), "deliveryMode": row.snapshot.delivery_mode,
        "deliveryKind": fields.get("deliveryKind"), "body": row.snapshot.content,
        "state": row.snapshot.state, "sentAt": sent_at(row),
    });
    if let Some(diagnostic) = fields.get("deliveryDiagnostics") { value["deliveryDiagnostics"] = diagnostic.clone(); }
    if let Some(meta) = &row.meta {
        if let Some(turn) = &meta.context_turn_id {
            value["contextTurnId"] = json!(turn);
        }
        if let Some(at) = &meta.delivered_at {
            value["deliveredAt"] = json!(at);
        }
        if meta.facts.wake_downgraded {
            value["wakeDowngraded"] = json!(true);
        }
        if let Some(turn) = &meta.facts.sender_turn_id {
            value["senderTurnId"] = json!(turn);
        }
    }
    if let Some(error) = &row.snapshot.error {
        value["error"] = error.clone();
    }
    if let Some(view) = settlement_view(row, Utc::now()) {
        value["settlement"] = json!(view);
    }
    Ok(value)
}

fn received_row(row: &StoredAgentBusMessage) -> anyhow::Result<Value> {
    let mut value = serde_json::to_value(&row.snapshot)?;
    if let Some(view) = settlement_view(row, Utc::now()) {
        value["settlement"] = json!(view);
    }
    let peer = stored_peer_tell_fields(row);
    if row.snapshot.from_cutex_session_id.is_some() || peer.is_some() {
        let object = value.as_object_mut().expect("snapshot is an object");
        object.extend(stored_agent_fields(row));
        object.extend(peer.unwrap_or_default());
        object.insert("queuedAt".into(), json!(sent_at(row)));
        if let Some(meta) = &row.meta {
            if let Some(turn) = &meta.context_turn_id {
                object.insert("contextTurnId".into(), json!(turn));
            }
            if let Some(at) = &meta.delivered_at {
                object.insert("deliveredAt".into(), json!(at));
            }
        }
    } else if let (Some(object), Some(canonical)) =
        (value.as_object_mut(), row.canonical_envelope.as_ref())
    {
        object.extend(system_sender_fields(canonical));
    }
    Ok(value)
}

// Legacy and opt-in modes deliberately share the serializers above; only
// record selection/order differs. Keep diagnostic optional fields intact.
pub(super) fn full_received(
    store: &AgentBusMessageStore,
    session: &str,
) -> anyhow::Result<Vec<Value>> {
    store
        .messages
        .values()
        .filter(|row| belongs(row, session, Direction::Received))
        .map(received_row)
        .collect()
}

pub(super) fn full_sent(
    store: &AgentBusMessageStore,
    session: &str,
    limit: usize,
) -> anyhow::Result<Vec<Value>> {
    let mut rows: Vec<_> = store
        .messages
        .values()
        .filter(|row| belongs(row, session, Direction::Sent))
        .collect();
    // Preserve legacy ordering, including its zero-time fallback.
    rows.sort_by_key(|row| {
        (
            row.canonical_envelope
                .as_ref()
                .map_or(0, |e| e.created_at_epoch_secs),
            row.snapshot.message_id.clone(),
        )
    });
    let skip = rows.len().saturating_sub(limit);
    rows.into_iter().skip(skip).map(sent_row).collect()
}

fn select<'a>(
    store: &'a AgentBusMessageStore,
    session: &str,
    direction: Direction,
    window: &Window,
) -> (Vec<&'a StoredAgentBusMessage>, bool) {
    let mut rows: Vec<_> = store
        .messages
        .values()
        .filter(|row| belongs(row, session, direction))
        .collect();
    rows.sort_by_cached_key(|row| key(row));
    let fallback: BTreeSet<_> = rows
        .iter()
        .rev()
        .filter(|row| !required(row) && turn(row, direction).is_none())
        .take(UNLINKED_LIMIT)
        .map(|row| row.snapshot.message_id.as_str())
        .collect();
    let selected: Vec<_> = rows
        .iter()
        .copied()
        .filter(|row| {
            required(row)
                || matches_window(row, direction, window)
                || fallback.contains(row.snapshot.message_id.as_str())
        })
        .collect();
    let more = selected.len() < rows.len();
    (selected, more)
}

impl AgentBusMessageRepository {
    pub fn bounded_bootstrap(&self, session: &str, window: &Window) -> anyhow::Result<Value> {
        self.read_matching(
            |db| super::super::hot_store::bus_history_rows(db, Some(session), Some(session)),
            |store| bounded(store, session, window),
        )
    }

    pub fn history_page(&self, session: &str, query: &Query) -> anyhow::Result<Value> {
        let directions = match query.direction {
            Direction::Received => (Some(session), None),
            Direction::Sent => (None, Some(session)),
        };
        self.read_matching(
            |db| super::super::hot_store::bus_history_rows(db, directions.0, directions.1),
            |store| page(store, session, query),
        )
    }
}

fn bounded(store: &AgentBusMessageStore, session: &str, window: &Window) -> anyhow::Result<Value> {
    let (received, received_more) = select(store, session, Direction::Received, window);
    let (sent, sent_more) = select(store, session, Direction::Sent, window);
    Ok(json!({
        "agentBusMessages": received.into_iter().map(|row| row_value(row, Direction::Received)).collect::<anyhow::Result<Vec<_>>>()?,
        "agentBusSentMessages": sent.into_iter().map(|row| row_value(row, Direction::Sent)).collect::<anyhow::Result<Vec<_>>>()?,
        "busMessagesPage": {"received": {"hasMore": received_more}, "sent": {"hasMore": sent_more}},
    }))
}

fn page(store: &AgentBusMessageStore, session: &str, query: &Query) -> anyhow::Result<Value> {
    let mut rows: Vec<_> = store
        .messages
        .values()
        .filter(|row| belongs(row, session, query.direction))
        .filter(|row| {
            query
                .window
                .as_ref()
                .is_none_or(|window| matches_window(row, query.direction, window))
        })
        .map(|row| (key(row), row))
        .filter(|(key, _)| query.before.as_ref().is_none_or(|before| key < before))
        .collect();
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    let more = rows.len() > query.limit;
    rows.truncate(query.limit);
    let next = if more {
        let cursor = Cursor {
            version: 1,
            session: session.into(),
            direction: query.direction,
            before: rows.last().expect("nonempty page").0.clone(),
            filter: filter_key(query.window.as_ref()),
        };
        Some(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&cursor)?))
    } else {
        None
    };
    rows.reverse();
    let messages = rows
        .into_iter()
        .map(|(_, row)| row_value(row, query.direction))
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(
        json!({"contractVersion": 2, "cutexSessionId": session, "direction": query.direction,
        "messages": messages, "page": {"limit": query.limit, "hasMore": more, "nextCursor": next}}),
    )
}

#[cfg(test)]
#[path = "agent_bus_history_tests.rs"]
mod tests;
