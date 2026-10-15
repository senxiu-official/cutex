//! A session's latest token usage, derived from its current native rollout
//! (no separate storage), so clients can show context usage right after a
//! (re)connect instead of waiting for the next live
//! `thread/tokenUsage/updated` notification.
//!
//! The rollout is scanned backwards for the newest `event_msg` of type
//! `token_count` whose `info` (native `TokenUsageInfo`:
//! `{total_token_usage, last_token_usage, model_context_window}`) is present
//! and well formed; rate-limit-only events (`info: null`) are skipped. The
//! enclosing turn is the nearest earlier turn marker (`turn_context.turn_id`
//! or `event_msg` `task_started`/`turn_started`).
//!
//! Reverted history stays consistent exactly as in `native_plan`: native
//! revert writes a new rollout whose `session_meta.history_base` names an
//! immutable prefix file and the byte length of it that remains, and legacy
//! `thread_rolled_back` markers drop the preceding turns.
//!
//! Wire shape (`cutexState.latestTokenUsage`) = the app-server's
//! `thread/tokenUsage/updated` params plus `updatedAt`:
//! `{threadId, turnId, tokenUsage: {total: Breakdown, last: Breakdown,
//!   modelContextWindow: number|null}, updatedAt}` or `null`, where
//! `Breakdown = {totalTokens, inputTokens, cachedInputTokens,
//! cacheWriteInputTokens, outputTokens, reasoningOutputTokens}`,
//! `updatedAt` is the rollout line's RFC 3339 `timestamp` verbatim and
//! `turnId` is null only for histories without turn markers.

use std::collections::{BTreeSet, HashMap};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use anyhow::ensure;
use serde_json::{json, Map, Value};

/// Upper bound on rollout bytes read per derivation (all segments).
pub const SCAN_BUDGET_BYTES: u64 = 256 * 1024 * 1024;
const CHUNK_BYTES: usize = 256 * 1024;
const CACHE_ENTRIES: usize = 128;
/// Lines that can matter; everything else is skipped without JSON parsing.
const NEEDLES: [&str; 5] = [
    "token_count",
    "turn_context",
    "task_started",
    "turn_started",
    "thread_rolled_back",
];

/// The latest token usage of native thread `thread_id` in `home`, or `None`.
pub fn latest_token_usage(home: &Path, thread_id: &str) -> anyhow::Result<Option<Value>> {
    let current = super::native_history::current(home, thread_id)?;
    latest_token_usage_in_rollout(home, &current, thread_id)
}

/// As [`latest_token_usage`] for an already resolved current rollout. Results
/// are cached per rollout until it changes (appends change its length).
pub fn latest_token_usage_in_rollout(
    home: &Path,
    current: &Path,
    thread_id: &str,
) -> anyhow::Result<Option<Value>> {
    let metadata = std::fs::metadata(current)?;
    let stamp = (metadata.len(), metadata.modified().ok());
    let key = (current.to_path_buf(), thread_id.to_string());
    if let Some(hit) = cache()
        .lock()
        .ok()
        .and_then(|cache| cache.get(&key).filter(|entry| entry.0 == stamp).map(|entry| entry.1.clone()))
    {
        return Ok(hit);
    }
    let result = scan_chain(home, current, stamp.0, thread_id, SCAN_BUDGET_BYTES)?;
    if let Ok(mut cache) = cache().lock() {
        if cache.len() >= CACHE_ENTRIES {
            cache.clear();
        }
        cache.insert(key, (stamp, result.clone()));
    }
    Ok(result)
}

type CacheKey = (PathBuf, String);
type CacheStamp = (u64, Option<SystemTime>);

fn cache() -> &'static Mutex<HashMap<CacheKey, (CacheStamp, Option<Value>)>> {
    static CACHE: OnceLock<Mutex<HashMap<CacheKey, (CacheStamp, Option<Value>)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn scan_chain(
    home: &Path,
    current: &Path,
    length: u64,
    thread_id: &str,
    budget: u64,
) -> anyhow::Result<Option<Value>> {
    let mut scanner = Scanner::default();
    let mut budget = budget;
    let mut seen = BTreeSet::new();
    let mut segment = Some((current.to_path_buf(), length));
    while let Some((path, end)) = segment {
        ensure!(seen.insert(path.clone()), "cyclic native history reference");
        if scan_segment(&path, end, &mut scanner, &mut budget)? {
            break;
        }
        segment = super::native_history::history_base(home, &path)?;
    }
    Ok(scanner.finish(thread_id))
}

/// Feeds `path[..end]` to `scanner` newest line first. Returns true when the
/// scan is complete (or the budget is spent).
fn scan_segment(
    path: &Path,
    end: u64,
    scanner: &mut Scanner,
    budget: &mut u64,
) -> anyhow::Result<bool> {
    let mut file = std::fs::File::open(path)?;
    let end = end.min(file.metadata()?.len());
    let mut position = end;
    // Bytes of the (possibly partial) line that ends at `position`'s tail.
    let mut carry: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; CHUNK_BYTES];
    while position > 0 {
        if *budget == 0 {
            return Ok(true);
        }
        let size = (position.min(CHUNK_BYTES as u64)).min(*budget) as usize;
        position -= size as u64;
        *budget -= size as u64;
        file.seek(SeekFrom::Start(position))?;
        file.read_exact(&mut chunk[..size])?;
        let mut tail = size;
        while let Some(newline) = chunk[..tail].iter().rposition(|&b| b == b'\n') {
            let mut line = chunk[newline + 1..tail].to_vec();
            line.extend_from_slice(&carry);
            carry.clear();
            if scanner.line(&line) {
                return Ok(true);
            }
            tail = newline;
        }
        let mut head = chunk[..tail].to_vec();
        head.extend_from_slice(&carry);
        carry = head;
    }
    Ok(scanner.line(&carry))
}

struct Found {
    token_usage: Value,
    updated_at: Value,
    turn_id: Option<String>,
}

#[derive(Default)]
struct Scanner {
    /// Turns dropped by legacy `thread_rolled_back` markers still ahead.
    rolled_back_turns: u64,
    found: Option<Found>,
}

impl Scanner {
    /// Consumes one line (newest first); true once the result is complete.
    fn line(&mut self, bytes: &[u8]) -> bool {
        let Ok(text) = std::str::from_utf8(bytes) else {
            return false;
        };
        if !NEEDLES.iter().any(|needle| text.contains(needle)) {
            return false;
        }
        let Ok(line) = serde_json::from_str::<Value>(text.trim()) else {
            return false;
        };
        let payload = &line["payload"];
        match (line["type"].as_str(), payload["type"].as_str()) {
            (Some("turn_context"), _) => {
                if let (Some(found), Some(turn)) = (&mut self.found, payload["turn_id"].as_str()) {
                    found.turn_id = Some(turn.to_string());
                    return true;
                }
            }
            (Some("event_msg"), Some("task_started" | "turn_started")) => {
                let turn = payload["turn_id"].as_str();
                if let Some(found) = &mut self.found {
                    if let Some(turn) = turn {
                        found.turn_id = Some(turn.to_string());
                        return true;
                    }
                } else if self.rolled_back_turns > 0 {
                    // The start of a dropped turn: older lines count again
                    // once every dropped turn is passed.
                    self.rolled_back_turns -= 1;
                }
            }
            (Some("event_msg"), Some("thread_rolled_back")) if self.found.is_none() => {
                self.rolled_back_turns += payload["num_turns"].as_u64().unwrap_or(0);
            }
            (Some("event_msg"), Some("token_count"))
                if self.found.is_none() && self.rolled_back_turns == 0 =>
            {
                if let Some(token_usage) = thread_token_usage(&payload["info"]) {
                    self.found = Some(Found {
                        token_usage,
                        updated_at: line["timestamp"].clone(),
                        turn_id: None,
                    });
                }
            }
            _ => {}
        }
        false
    }

    fn finish(self, thread_id: &str) -> Option<Value> {
        self.found.map(|found| {
            json!({
                "threadId": thread_id,
                "turnId": found.turn_id,
                "tokenUsage": found.token_usage,
                "updatedAt": found.updated_at,
            })
        })
    }
}

/// Native `TokenUsageInfo` → app-server v2 `ThreadTokenUsage`
/// (`{total, last, modelContextWindow}`); `None` when absent or malformed.
fn thread_token_usage(info: &Value) -> Option<Value> {
    let info = info.as_object()?;
    let total = token_usage_breakdown(info.get("total_token_usage")?)?;
    let last = token_usage_breakdown(info.get("last_token_usage")?)?;
    let model_context_window = match info.get("model_context_window") {
        None | Some(Value::Null) => Value::Null,
        Some(window) => Value::from(window.as_i64()?),
    };
    Some(json!({
        "total": total,
        "last": last,
        "modelContextWindow": model_context_window,
    }))
}

/// Native `TokenUsage` (snake_case; `cache_write_input_tokens` defaults to
/// 0 for older rollouts) → v2 `TokenUsageBreakdown` (camelCase).
fn token_usage_breakdown(usage: &Value) -> Option<Value> {
    let usage = usage.as_object()?;
    let mut breakdown = Map::new();
    for (native, wire, required) in [
        ("total_tokens", "totalTokens", true),
        ("input_tokens", "inputTokens", true),
        ("cached_input_tokens", "cachedInputTokens", true),
        ("cache_write_input_tokens", "cacheWriteInputTokens", false),
        ("output_tokens", "outputTokens", true),
        ("reasoning_output_tokens", "reasoningOutputTokens", true),
    ] {
        let value = match usage.get(native) {
            Some(value) => value.as_i64()?,
            None if !required => 0,
            None => return None,
        };
        breakdown.insert(wire.to_string(), Value::from(value));
    }
    Some(Value::Object(breakdown))
}

#[cfg(test)]
mod tests {
    use super::*;

    const THREAD: &str = "0189a000-0000-7000-8000-000000000002";

    struct Dir(PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn home() -> Dir {
        let root =
            std::env::temp_dir().join(format!("cutex-token-usage-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("sessions").join("2026")).unwrap();
        Dir(root)
    }

    fn meta(base: Value) -> Value {
        json!({"timestamp":"2026-10-07T00:00:00.000Z","type":"session_meta",
               "payload":{"id":THREAD,"history_base":base}})
    }
    fn turn(id: &str, at: &str) -> Vec<Value> {
        vec![
            json!({"timestamp":at,"type":"event_msg","payload":{"type":"task_started","turn_id":id}}),
            json!({"timestamp":at,"type":"turn_context","payload":{"turn_id":id,"cwd":"/w","model":"m"}}),
            json!({"timestamp":at,"type":"response_item","payload":{"type":"message","role":"user",
                   "content":[{"type":"input_text","text":"go"}]}}),
        ]
    }
    fn native_usage(total: i64) -> Value {
        json!({"input_tokens":total - 10,"cached_input_tokens":total / 2,"cache_write_input_tokens":0,
               "output_tokens":10,"reasoning_output_tokens":3,"total_tokens":total})
    }
    fn wire_usage(total: i64) -> Value {
        json!({"totalTokens":total,"inputTokens":total - 10,"cachedInputTokens":total / 2,
               "cacheWriteInputTokens":0,"outputTokens":10,"reasoningOutputTokens":3})
    }
    /// A real-shaped native `token_count` event (as written by cute-codex r48).
    fn count(at: &str, total: i64, last: i64, window: Value) -> Value {
        json!({"timestamp":at,"ordinal":1,"type":"event_msg","payload":{"type":"token_count",
               "info":{"total_token_usage":native_usage(total),"last_token_usage":native_usage(last),
                       "model_context_window":window},
               "rate_limits":{"limit_id":"codex","limit_name":null,"primary":null,"secondary":null,
                              "credits":null,"individual_limit":null,"spend_control_reached":null,
                              "plan_type":null,"rate_limit_reached_type":null}}})
    }
    fn write(path: &Path, lines: &[Value]) -> u64 {
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(path, &text).unwrap();
        text.len() as u64
    }
    fn scan(home: &Path, path: &Path) -> Option<Value> {
        let length = std::fs::metadata(path).unwrap().len();
        scan_chain(home, path, length, THREAD, SCAN_BUDGET_BYTES).unwrap()
    }

    #[test]
    fn newest_token_count_wins_in_the_app_server_wire_shape() {
        let home = home();
        let path = home.0.join("sessions/2026").join(format!("rollout-x-{THREAD}.jsonl"));
        let mut lines = vec![meta(Value::Null)];
        lines.extend(turn("turn-1", "2026-10-07T01:00:00.000Z"));
        lines.push(count("2026-10-07T01:00:05.000Z", 1000, 1000, json!(258400)));
        lines.extend(turn("turn-2", "2026-10-07T02:00:00.000Z"));
        lines.push(count("2026-10-07T02:00:05.000Z", 3000, 2000, json!(258400)));
        // A rate-limit-only event after it does not hide the usage.
        lines.push(json!({"timestamp":"2026-10-07T02:00:06.000Z","type":"event_msg","payload":{
            "type":"token_count","info":null,"rate_limits":null}}));
        lines.push(json!({"timestamp":"2026-10-07T02:00:07.000Z","type":"response_item",
            "payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}));
        write(&path, &lines);
        let expected = json!({
            "threadId": THREAD, "turnId": "turn-2",
            "tokenUsage": {"total": wire_usage(3000), "last": wire_usage(2000), "modelContextWindow": 258400},
            "updatedAt": "2026-10-07T02:00:05.000Z",
        });
        assert_eq!(scan(&home.0, &path), Some(expected.clone()));
        // The public entry point agrees (also from its cache), and an append
        // invalidates the cached answer.
        assert_eq!(latest_token_usage_in_rollout(&home.0, &path, THREAD).unwrap(), Some(expected.clone()));
        assert_eq!(latest_token_usage_in_rollout(&home.0, &path, THREAD).unwrap(), Some(expected));
        lines.push(count("2026-10-07T02:01:00.000Z", 5000, 2000, Value::Null));
        write(&path, &lines);
        let found = latest_token_usage_in_rollout(&home.0, &path, THREAD).unwrap().unwrap();
        assert_eq!(found["tokenUsage"]["total"], wire_usage(5000));
        assert_eq!(found["tokenUsage"]["modelContextWindow"], Value::Null);
        assert_eq!(found["turnId"], "turn-2");
    }

    #[test]
    fn no_usage_and_malformed_lines_yield_none_or_older_usage() {
        let home = home();
        let path = home.0.join("sessions/2026").join(format!("rollout-x-{THREAD}.jsonl"));
        let mut lines = vec![meta(Value::Null)];
        lines.extend(turn("turn-1", "2026-10-07T01:00:00.000Z"));
        lines.push(json!({"timestamp":"2026-10-07T01:00:01.000Z","type":"event_msg","payload":{
            "type":"token_count","info":null,"rate_limits":null}}));
        write(&path, &lines);
        assert_eq!(scan(&home.0, &path), None);

        // Older rollouts without cache_write_input_tokens are accepted.
        let mut legacy = native_usage(400);
        legacy.as_object_mut().unwrap().remove("cache_write_input_tokens");
        lines.push(json!({"timestamp":"2026-10-07T01:00:02.000Z","type":"event_msg","payload":{
            "type":"token_count","info":{"total_token_usage":legacy,"last_token_usage":legacy}}}));
        // Malformed events and torn / garbage lines are skipped.
        for info in [
            json!({"total_token_usage":native_usage(9)}),
            json!({"total_token_usage":{"input_tokens":"many"},"last_token_usage":native_usage(9)}),
            json!({"total_token_usage":native_usage(9),"last_token_usage":native_usage(9),
                   "model_context_window":"wide"}),
            json!("token_count"),
        ] {
            lines.push(json!({"timestamp":"2026-10-07T01:00:03.000Z","type":"event_msg",
                "payload":{"type":"token_count","info":info}}));
        }
        lines.push(json!("garbage token_count line"));
        let length = write(&path, &lines);
        let mut text = std::fs::read(&path).unwrap();
        text.extend_from_slice(br#"{"timestamp":"2026-10-07T01:00:04.000Z","type":"event_msg","payload":{"type":"token_cou"#);
        std::fs::write(&path, text).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() > length);
        let found = scan(&home.0, &path).expect("the well-formed legacy usage");
        assert_eq!(found["turnId"], "turn-1");
        assert_eq!(found["updatedAt"], "2026-10-07T01:00:02.000Z");
        assert_eq!(found["tokenUsage"]["total"]["cacheWriteInputTokens"], 0);
        assert_eq!(found["tokenUsage"]["total"]["totalTokens"], 400);
        assert_eq!(found["tokenUsage"]["modelContextWindow"], Value::Null);
    }

    #[test]
    fn reverted_history_reads_only_the_retained_prefix() {
        let home = home();
        let rollout_id = "0189a000-0000-7000-8000-0000000000fe";
        let old = home.0.join("sessions/2026").join(format!("rollout-a-{THREAD}.jsonl"));
        let mut lines = vec![meta(Value::Null)];
        lines.extend(turn("turn-1", "2026-10-07T01:00:00.000Z"));
        lines.push(count("2026-10-07T01:00:05.000Z", 1000, 1000, json!(100000)));
        let retained: String = lines.iter().map(|l| format!("{l}\n")).collect();
        // turn-2 (with newer usage) is reverted away.
        lines.extend(turn("turn-2", "2026-10-07T02:00:00.000Z"));
        lines.push(count("2026-10-07T02:00:05.000Z", 9000, 8000, json!(100000)));
        write(&old, &lines);
        let current = home
            .0
            .join("sessions/2026")
            .join(format!("rollout-b-{THREAD}_{rollout_id}.jsonl"));
        let base = json!({"thread_id":THREAD,"end_ordinal_exclusive":4,"end_byte_offset":retained.len()});
        let mut new_lines = vec![meta(base)];
        new_lines.extend(turn("turn-3", "2026-10-07T03:00:00.000Z"));
        write(&current, &new_lines);
        let found = scan(&home.0, &current).expect("usage from the retained prefix");
        assert_eq!(found["turnId"], "turn-1");
        assert_eq!(found["tokenUsage"]["total"], wire_usage(1000));

        // Newer usage in the replacement history wins.
        new_lines.push(count("2026-10-07T03:00:05.000Z", 1500, 500, json!(100000)));
        write(&current, &new_lines);
        let found = scan(&home.0, &current).unwrap();
        assert_eq!(found["turnId"], "turn-3");
        assert_eq!(found["tokenUsage"]["last"], wire_usage(500));
    }

    #[test]
    fn latest_token_usage_follows_the_native_index_to_the_current_rollout() {
        let home = home();
        let old = home.0.join("sessions/2026").join(format!("rollout-a-{THREAD}.jsonl"));
        let mut lines = vec![meta(Value::Null)];
        lines.extend(turn("turn-1", "2026-10-07T01:00:00.000Z"));
        lines.push(count("2026-10-07T01:00:05.000Z", 1000, 1000, json!(100000)));
        write(&old, &lines);
        let current = home.0.join("sessions/2026").join(format!(
            "rollout-b-{THREAD}_0189a000-0000-7000-8000-0000000000ab.jsonl"
        ));
        let mut new_lines =
            vec![meta(json!({"thread_id":THREAD,"end_ordinal_exclusive":0,"end_byte_offset":0}))];
        new_lines.extend(turn("turn-9", "2026-10-07T09:00:00.000Z"));
        write(&current, &new_lines);
        assert!(latest_token_usage(&home.0, THREAD).is_err());
        let connection = rusqlite::Connection::open(home.0.join("state_5.sqlite")).unwrap();
        connection.execute_batch("CREATE TABLE threads (id TEXT, rollout_path TEXT)").unwrap();
        connection
            .execute("INSERT INTO threads VALUES (?1,?2)", [THREAD, current.to_str().unwrap()])
            .unwrap();
        drop(connection);
        assert_eq!(latest_token_usage(&home.0, THREAD).unwrap(), None);
        new_lines.push(count("2026-10-07T09:00:05.000Z", 700, 700, json!(100000)));
        write(&current, &new_lines);
        let found = latest_token_usage(&home.0, THREAD).unwrap().unwrap();
        assert_eq!(found["threadId"], THREAD);
        assert_eq!(found["turnId"], "turn-9");
        assert_eq!(found["tokenUsage"]["total"], wire_usage(700));
    }

    #[test]
    fn legacy_rollback_markers_drop_their_turns() {
        let home = home();
        let path = home.0.join("sessions/2026").join(format!("rollout-x-{THREAD}.jsonl"));
        let mut lines = vec![meta(Value::Null)];
        lines.extend(turn("turn-1", "2026-10-07T01:00:00.000Z"));
        lines.push(count("2026-10-07T01:00:05.000Z", 1000, 1000, json!(100000)));
        lines.extend(turn("turn-2", "2026-10-07T02:00:00.000Z"));
        lines.push(count("2026-10-07T02:00:05.000Z", 9000, 8000, json!(100000)));
        lines.push(json!({"timestamp":"2026-10-07T02:30:00.000Z","type":"event_msg",
            "payload":{"type":"thread_rolled_back","num_turns":1}}));
        write(&path, &lines);
        let found = scan(&home.0, &path).unwrap();
        assert_eq!(found["turnId"], "turn-1");
        assert_eq!(found["tokenUsage"]["total"], wire_usage(1000));
    }

    #[test]
    fn the_budget_bounds_the_scan() {
        let home = home();
        let path = home.0.join("sessions/2026").join(format!("rollout-x-{THREAD}.jsonl"));
        let mut lines = vec![meta(Value::Null)];
        lines.extend(turn("turn-1", "2026-10-07T01:00:00.000Z"));
        lines.push(count("2026-10-07T01:00:05.000Z", 1000, 1000, json!(100000)));
        let filler = "f".repeat(CHUNK_BYTES + 3);
        for _ in 0..3 {
            lines.push(json!({"timestamp":"2026-10-07T01:00:06.000Z","type":"response_item","payload":{
                "type":"function_call_output","call_id":"x","output":filler}}));
        }
        let length = write(&path, &lines);
        assert_eq!(scan(&home.0, &path).unwrap()["turnId"], "turn-1");
        let none = scan_chain(&home.0, &path, length, THREAD, (CHUNK_BYTES as u64) * 3).unwrap();
        assert_eq!(none, None);
    }
}
