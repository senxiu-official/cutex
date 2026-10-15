//! A session's latest accepted `update_plan` checklist, derived from its
//! current native rollout (no separate storage, so it also covers history
//! written before cutex knew about plans).
//!
//! The rollout is scanned backwards for the newest `response_item`
//! `function_call` named `update_plan` whose arguments parse as the native
//! `UpdatePlanArgs` (`{explanation?, plan: [{step, status}]}`) and whose
//! `function_call_output` (same `call_id`) is the native success text
//! `Plan updated`; rejected calls (Plan mode, bad arguments, no output) are
//! skipped. The enclosing turn is the nearest earlier turn marker
//! (`turn_context.turn_id` or `event_msg` `task_started`/`turn_started`).
//!
//! Reverted history stays consistent: native revert writes a new rollout
//! whose `session_meta.history_base` names an immutable prefix file and the
//! byte length of it that remains, so dropped turns are never read. Legacy
//! `thread_rolled_back` markers drop the preceding turns as well.
//!
//! Wire shape (`cutexState.latestPlan`):
//! `{threadId, turnId, explanation: string|null,
//!   plan: [{step, status: "pending"|"inProgress"|"completed"}],
//!   updatedAt}` or `null`, where `updatedAt` is the rollout line's
//! RFC 3339 `timestamp` verbatim and `turnId` is null only for histories
//! without turn markers.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use anyhow::ensure;
use serde_json::{json, Value};

/// Native `PlanToolOutput` text for an accepted `update_plan` call.
pub const PLAN_UPDATED_OUTPUT: &str = "Plan updated";
/// Upper bound on rollout bytes read per derivation (all segments).
pub const SCAN_BUDGET_BYTES: u64 = 256 * 1024 * 1024;
const CHUNK_BYTES: usize = 256 * 1024;
const CACHE_ENTRIES: usize = 128;
/// Lines that can matter; everything else is skipped without JSON parsing.
const NEEDLES: [&str; 6] = [
    "update_plan",
    PLAN_UPDATED_OUTPUT,
    "turn_context",
    "task_started",
    "turn_started",
    "thread_rolled_back",
];

/// The latest plan of native thread `thread_id` in `home`, or `None`.
pub fn latest_plan(home: &Path, thread_id: &str) -> anyhow::Result<Option<Value>> {
    let current = super::native_history::current(home, thread_id)?;
    latest_plan_in_rollout(home, &current, thread_id)
}

/// As [`latest_plan`] for an already resolved current rollout. Results are
/// cached per rollout until it changes (appends change its length).
pub fn latest_plan_in_rollout(
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
    explanation: Option<String>,
    plan: Vec<Value>,
    updated_at: Value,
    turn_id: Option<String>,
}

#[derive(Default)]
struct Scanner {
    /// `call_id`s whose output (seen later in the history) is the success text.
    accepted_calls: HashSet<String>,
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
            (Some("response_item"), Some("function_call_output")) if self.found.is_none() => {
                if let Some(call_id) = payload["call_id"].as_str() {
                    if is_plan_updated(&payload["output"]) {
                        self.accepted_calls.insert(call_id.to_string());
                    }
                }
            }
            (Some("response_item"), Some("function_call"))
                if self.found.is_none() && self.rolled_back_turns == 0 =>
            {
                let plain_tool = payload["name"] == "update_plan"
                    && payload.get("namespace").is_none_or(Value::is_null);
                let accepted = payload["call_id"]
                    .as_str()
                    .is_some_and(|call_id| self.accepted_calls.contains(call_id));
                if plain_tool && accepted {
                    if let Some((explanation, plan)) =
                        payload["arguments"].as_str().and_then(parse_update_plan_arguments)
                    {
                        self.found = Some(Found {
                            explanation,
                            plan,
                            updated_at: line["timestamp"].clone(),
                            turn_id: None,
                        });
                    }
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
                "explanation": found.explanation,
                "plan": found.plan,
                "updatedAt": found.updated_at,
            })
        })
    }
}

/// The native success output: a plain string, structured text content
/// items, or the older `{content, success}` object.
fn is_plan_updated(output: &Value) -> bool {
    match output {
        Value::String(text) => text == PLAN_UPDATED_OUTPUT,
        Value::Array(items) => {
            let mut text = String::new();
            for item in items {
                match item["text"].as_str() {
                    Some(part) => text.push_str(part),
                    None => return false,
                }
            }
            text == PLAN_UPDATED_OUTPUT
        }
        Value::Object(object) => {
            object.get("content").and_then(Value::as_str) == Some(PLAN_UPDATED_OUTPUT)
                && object.get("success").is_none_or(|success| success != &Value::Bool(false))
        }
        _ => false,
    }
}

/// Native `UpdatePlanArgs` (`deny_unknown_fields`): `explanation` optional
/// string, `plan` a list of exactly `{step: string, status}` with snake_case
/// statuses, returned camelCase.
fn parse_update_plan_arguments(arguments: &str) -> Option<(Option<String>, Vec<Value>)> {
    let value: Value = serde_json::from_str(arguments).ok()?;
    let object = value.as_object()?;
    if object.keys().any(|key| key != "explanation" && key != "plan") {
        return None;
    }
    let explanation = match object.get("explanation") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => return None,
    };
    let plan = object
        .get("plan")?
        .as_array()?
        .iter()
        .map(|item| {
            let item = item.as_object()?;
            if item.len() != 2 {
                return None;
            }
            let step = item.get("step")?.as_str()?;
            let status = match item.get("status")?.as_str()? {
                "pending" => "pending",
                "in_progress" => "inProgress",
                "completed" => "completed",
                _ => return None,
            };
            Some(json!({ "step": step, "status": status }))
        })
        .collect::<Option<Vec<_>>>()?;
    Some((explanation, plan))
}

#[cfg(test)]
mod tests {
    use super::*;

    const THREAD: &str = "0189a000-0000-7000-8000-000000000001";

    struct Dir(PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn home() -> Dir {
        let root = std::env::temp_dir().join(format!("cutex-plan-test-{}", uuid::Uuid::new_v4()));
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
    fn call(call_id: &str, at: &str, arguments: Value) -> Value {
        let arguments = match arguments {
            Value::String(raw) => raw,
            other => other.to_string(),
        };
        json!({"timestamp":at,"type":"response_item","payload":{"type":"function_call",
               "name":"update_plan","arguments":arguments,"call_id":call_id}})
    }
    fn output(call_id: &str, at: &str, text: &str) -> Value {
        json!({"timestamp":at,"type":"response_item","payload":{"type":"function_call_output",
               "call_id":call_id,"output":text}})
    }
    fn plan(explanation: Option<&str>, steps: &[(&str, &str)]) -> Value {
        let plan: Vec<_> = steps.iter().map(|(s, st)| json!({"step":s,"status":st})).collect();
        match explanation {
            Some(e) => json!({"explanation":e,"plan":plan}),
            None => json!({"plan":plan}),
        }
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
    fn latest_accepted_plan_with_turn_and_camel_case_statuses() {
        let home = home();
        let path = home.0.join("sessions/2026").join(format!("rollout-x-{THREAD}.jsonl"));
        let mut lines = vec![meta(Value::Null)];
        lines.extend(turn("turn-1", "2026-10-07T01:00:00.000Z"));
        lines.push(call("c1", "2026-10-07T01:00:01.000Z", plan(None, &[("old", "pending")])));
        lines.push(output("c1", "2026-10-07T01:00:01.500Z", "Plan updated"));
        lines.extend(turn("turn-2", "2026-10-07T02:00:00.000Z"));
        lines.push(call(
            "c2",
            "2026-10-07T02:00:01.000Z",
            plan(Some("why"), &[("a", "completed"), ("b", "in_progress"), ("c", "pending")]),
        ));
        lines.push(output("c2", "2026-10-07T02:00:01.500Z", "Plan updated"));
        lines.push(json!({"timestamp":"2026-10-07T02:00:02.000Z","type":"response_item",
            "payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}));
        write(&path, &lines);
        assert_eq!(
            scan(&home.0, &path),
            Some(json!({
                "threadId": THREAD, "turnId": "turn-2", "explanation": "why",
                "plan": [{"step":"a","status":"completed"},{"step":"b","status":"inProgress"},
                         {"step":"c","status":"pending"}],
                "updatedAt": "2026-10-07T02:00:01.000Z",
            }))
        );
        // The public entry point agrees, also when answered from its cache,
        // and an append invalidates the cached answer.
        assert_eq!(latest_plan_in_rollout(&home.0, &path, THREAD).unwrap(), scan(&home.0, &path));
        assert_eq!(latest_plan_in_rollout(&home.0, &path, THREAD).unwrap(), scan(&home.0, &path));
        lines.push(call("c3", "2026-10-07T03:00:00.000Z", plan(None, &[("next", "pending")])));
        lines.push(output("c3", "2026-10-07T03:00:00.500Z", "Plan updated"));
        write(&path, &lines);
        let cached = latest_plan_in_rollout(&home.0, &path, THREAD).unwrap().unwrap();
        assert_eq!(cached["plan"], json!([{"step":"next","status":"pending"}]));
        assert_eq!(cached["turnId"], "turn-2");
    }

    #[test]
    fn rejected_calls_are_skipped_for_an_earlier_success() {
        let home = home();
        let path = home.0.join("sessions/2026").join(format!("rollout-x-{THREAD}.jsonl"));
        let mut lines = vec![meta(Value::Null)];
        lines.extend(turn("turn-1", "2026-10-07T01:00:00.000Z"));
        lines.push(call("ok", "2026-10-07T01:00:01.000Z", plan(None, &[("kept", "in_progress")])));
        lines.push(output("ok", "2026-10-07T01:00:02.000Z", "Plan updated"));
        lines.extend(turn("turn-2", "2026-10-07T02:00:00.000Z"));
        // Plan mode refusal, parse failure reported by native, and a call
        // whose output never arrived.
        lines.push(call("plan-mode", "2026-10-07T02:00:01.000Z", plan(None, &[("x", "pending")])));
        lines.push(output(
            "plan-mode",
            "2026-10-07T02:00:01.100Z",
            "update_plan is a TODO/checklist tool and is not allowed in Plan mode",
        ));
        lines.push(call("bad", "2026-10-07T02:00:02.000Z", json!({"plan":[{"step":"x","status":"done"}]})));
        lines.push(output("bad", "2026-10-07T02:00:02.100Z", "failed to parse function arguments: x"));
        lines.push(call("pending", "2026-10-07T02:00:03.000Z", plan(None, &[("y", "pending")])));
        // Another tool's success text does not vouch for update_plan.
        lines.push(json!({"timestamp":"2026-10-07T02:00:04.000Z","type":"response_item","payload":{
            "type":"function_call","name":"other_tool","namespace":null,"arguments":"{}","call_id":"other"}}));
        lines.push(output("other", "2026-10-07T02:00:04.100Z", "Plan updated"));
        write(&path, &lines);
        let found = scan(&home.0, &path).expect("earlier accepted plan");
        assert_eq!(found["turnId"], "turn-1");
        assert_eq!(found["explanation"], Value::Null);
        assert_eq!(found["plan"], json!([{"step":"kept","status":"inProgress"}]));
        assert_eq!(found["updatedAt"], "2026-10-07T01:00:01.000Z");
    }

    #[test]
    fn malformed_arguments_and_histories_without_plans_yield_none() {
        let home = home();
        let path = home.0.join("sessions/2026").join(format!("rollout-x-{THREAD}.jsonl"));
        let mut lines = vec![meta(Value::Null)];
        lines.extend(turn("turn-1", "2026-10-07T01:00:00.000Z"));
        for (index, arguments) in [
            Value::String("not json".into()),
            json!({"plan":"steps"}),
            json!({"plan":[{"step":"a","status":"pending","extra":1}]}),
            json!({"plan":[{"step":1,"status":"pending"}]}),
            json!({"plan":[{"step":"a","status":"inProgress"}]}),
            json!({"plan":[{"step":"a","status":"pending"}],"explanation":5}),
            json!({"plan":[{"step":"a","status":"pending"}],"unknown":true}),
            json!({"explanation":"no plan"}),
        ]
        .into_iter()
        .enumerate()
        {
            let id = format!("c{index}");
            lines.push(call(&id, "2026-10-07T01:00:01.000Z", arguments));
            // Even with a success output, unparseable arguments are skipped.
            lines.push(output(&id, "2026-10-07T01:00:01.100Z", "Plan updated"));
        }
        lines.push(json!("garbage line"));
        write(&path, &lines);
        assert_eq!(scan(&home.0, &path), None);

        let empty = home.0.join("sessions/2026").join("rollout-empty.jsonl");
        let mut lines = vec![meta(Value::Null)];
        lines.extend(turn("turn-1", "2026-10-07T01:00:00.000Z"));
        write(&empty, &lines);
        assert_eq!(scan(&home.0, &empty), None);
    }

    #[test]
    fn empty_plan_null_explanation_structured_output_and_partial_tail() {
        let home = home();
        let path = home.0.join("sessions/2026").join(format!("rollout-x-{THREAD}.jsonl"));
        let mut lines = vec![meta(Value::Null)];
        lines.extend(turn("turn-1", "2026-10-07T01:00:00.000Z"));
        lines.push(call("c1", "2026-10-07T01:00:01.000Z", json!({"explanation":null,"plan":[]})));
        lines.push(json!({"timestamp":"2026-10-07T01:00:02.000Z","type":"response_item","payload":{
            "type":"function_call_output","call_id":"c1",
            "output":[{"type":"input_text","text":"Plan updated"}]}}));
        let length = write(&path, &lines);
        // A torn final line (a live writer mid-append) is ignored.
        let mut text = std::fs::read(&path).unwrap();
        text.extend_from_slice(br#"{"timestamp":"2026-10-07T01:00:03.000Z","type":"respon"#);
        std::fs::write(&path, text).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() > length);
        let found = scan(&home.0, &path).unwrap();
        assert_eq!(found["plan"], json!([]));
        assert_eq!(found["explanation"], Value::Null);
        assert_eq!(found["turnId"], "turn-1");
    }

    #[test]
    fn reverted_history_reads_only_the_retained_prefix() {
        let home = home();
        let rollout_id = "0189a000-0000-7000-8000-0000000000ff";
        let old = home.0.join("sessions/2026").join(format!("rollout-a-{THREAD}.jsonl"));
        let mut lines = vec![meta(Value::Null)];
        lines.extend(turn("turn-1", "2026-10-07T01:00:00.000Z"));
        lines.push(call("c1", "2026-10-07T01:00:01.000Z", plan(None, &[("kept", "completed")])));
        lines.push(output("c1", "2026-10-07T01:00:02.000Z", "Plan updated"));
        let retained: String = lines.iter().map(|l| format!("{l}\n")).collect();
        // turn-2 (with a newer plan) is reverted away.
        lines.extend(turn("turn-2", "2026-10-07T02:00:00.000Z"));
        lines.push(call("c2", "2026-10-07T02:00:01.000Z", plan(None, &[("dropped", "pending")])));
        lines.push(output("c2", "2026-10-07T02:00:02.000Z", "Plan updated"));
        write(&old, &lines);
        let current = home
            .0
            .join("sessions/2026")
            .join(format!("rollout-b-{THREAD}_{rollout_id}.jsonl"));
        let base = json!({"thread_id":THREAD,"end_ordinal_exclusive":5,"end_byte_offset":retained.len()});
        let mut new_lines = vec![meta(base)];
        new_lines.extend(turn("turn-3", "2026-10-07T03:00:00.000Z"));
        write(&current, &new_lines);
        let found = scan(&home.0, &current).expect("plan from the retained prefix");
        assert_eq!(found["turnId"], "turn-1");
        assert_eq!(found["plan"], json!([{"step":"kept","status":"completed"}]));

        // A newer plan in the replacement history wins without reading the prefix.
        new_lines.push(call("c3", "2026-10-07T03:00:01.000Z", plan(None, &[("new", "pending")])));
        new_lines.push(output("c3", "2026-10-07T03:00:02.000Z", "Plan updated"));
        write(&current, &new_lines);
        let found = scan(&home.0, &current).unwrap();
        assert_eq!(found["turnId"], "turn-3");
        assert_eq!(found["plan"], json!([{"step":"new","status":"pending"}]));
    }

    #[test]
    fn latest_plan_follows_the_native_index_to_the_current_rollout() {
        let home = home();
        let old = home.0.join("sessions/2026").join(format!("rollout-a-{THREAD}.jsonl"));
        let mut lines = vec![meta(Value::Null)];
        lines.extend(turn("turn-1", "2026-10-07T01:00:00.000Z"));
        lines.push(call("c1", "2026-10-07T01:00:01.000Z", plan(None, &[("old file", "pending")])));
        lines.push(output("c1", "2026-10-07T01:00:02.000Z", "Plan updated"));
        write(&old, &lines);
        // A reverted replacement that keeps none of the old file.
        let current = home.0.join("sessions/2026").join(format!(
            "rollout-b-{THREAD}_0189a000-0000-7000-8000-0000000000aa.jsonl"
        ));
        let mut new_lines =
            vec![meta(json!({"thread_id":THREAD,"end_ordinal_exclusive":0,"end_byte_offset":0}))];
        new_lines.extend(turn("turn-9", "2026-10-07T09:00:00.000Z"));
        write(&current, &new_lines);
        // Without the native index the current rollout is ambiguous.
        assert!(latest_plan(&home.0, THREAD).is_err());
        let connection = rusqlite::Connection::open(home.0.join("state_5.sqlite")).unwrap();
        connection.execute_batch("CREATE TABLE threads (id TEXT, rollout_path TEXT)").unwrap();
        connection
            .execute("INSERT INTO threads VALUES (?1,?2)", [THREAD, current.to_str().unwrap()])
            .unwrap();
        drop(connection);
        assert_eq!(latest_plan(&home.0, THREAD).unwrap(), None);
        new_lines.push(call("c2", "2026-10-07T09:00:01.000Z", plan(None, &[("current", "completed")])));
        new_lines.push(output("c2", "2026-10-07T09:00:02.000Z", "Plan updated"));
        write(&current, &new_lines);
        let found = latest_plan(&home.0, THREAD).unwrap().unwrap();
        assert_eq!(found["threadId"], THREAD);
        assert_eq!(found["turnId"], "turn-9");
        assert_eq!(found["plan"], json!([{"step":"current","status":"completed"}]));
    }

    #[test]
    fn legacy_rollback_markers_drop_their_turns() {
        let home = home();
        let path = home.0.join("sessions/2026").join(format!("rollout-x-{THREAD}.jsonl"));
        let mut lines = vec![meta(Value::Null)];
        lines.extend(turn("turn-1", "2026-10-07T01:00:00.000Z"));
        lines.push(call("c1", "2026-10-07T01:00:01.000Z", plan(None, &[("kept", "pending")])));
        lines.push(output("c1", "2026-10-07T01:00:02.000Z", "Plan updated"));
        lines.extend(turn("turn-2", "2026-10-07T02:00:00.000Z"));
        lines.push(call("c2", "2026-10-07T02:00:01.000Z", plan(None, &[("dropped", "pending")])));
        lines.push(output("c2", "2026-10-07T02:00:02.000Z", "Plan updated"));
        lines.push(json!({"timestamp":"2026-10-07T02:30:00.000Z","type":"event_msg",
            "payload":{"type":"thread_rolled_back","num_turns":1}}));
        write(&path, &lines);
        let found = scan(&home.0, &path).unwrap();
        assert_eq!(found["turnId"], "turn-1");
        assert_eq!(found["plan"], json!([{"step":"kept","status":"pending"}]));
    }

    #[test]
    fn long_lines_span_chunks_and_the_budget_bounds_the_scan() {
        let home = home();
        let path = home.0.join("sessions/2026").join(format!("rollout-x-{THREAD}.jsonl"));
        let mut lines = vec![meta(Value::Null)];
        lines.extend(turn("turn-1", "2026-10-07T01:00:00.000Z"));
        let long_step = "s".repeat(CHUNK_BYTES * 2 + 17);
        lines.push(call("c1", "2026-10-07T01:00:01.000Z", plan(None, &[(&long_step, "pending")])));
        lines.push(output("c1", "2026-10-07T01:00:02.000Z", "Plan updated"));
        let filler = "f".repeat(CHUNK_BYTES + 3);
        for _ in 0..3 {
            lines.push(json!({"timestamp":"2026-10-07T01:00:03.000Z","type":"response_item","payload":{
                "type":"function_call_output","call_id":"x","output":filler}}));
        }
        let length = write(&path, &lines);
        let found = scan(&home.0, &path).unwrap();
        assert_eq!(found["plan"][0]["step"].as_str().map(str::len), Some(long_step.len()));
        assert_eq!(found["turnId"], "turn-1");
        // A budget that only covers the filler finds nothing (and fails soft).
        let none = scan_chain(&home.0, &path, length, THREAD, (CHUNK_BYTES as u64) * 3).unwrap();
        assert_eq!(none, None);
    }
}
