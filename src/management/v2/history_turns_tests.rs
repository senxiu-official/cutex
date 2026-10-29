use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use base64::Engine;
use serde_json::{json, Value};

use super::super::history::{respond_read_shaped_with, respond_read_with, HistoryCallError, HistoryQuery};
use super::super::media_refs::{self, MediaStore, CACHE_MAX_BYTES, INLINE_MAX, TEXT_MAX};
use super::*;

/// A native thread: turns oldest first, every call recorded.
#[derive(Default)]
struct Native {
    turns: Vec<Value>,
    calls: Vec<(String, Value)>,
    /// `thread/turns/list itemsView=notLoaded` is rejected (older runtime).
    no_not_loaded: bool,
    /// No `thread/turns/list` at all (legacy `thread/read includeTurns`).
    no_turns_list: bool,
    /// Appended right after the next notLoaded listing (a turn that starts
    /// between the listing and the full read).
    append_after_listing: Option<Value>,
}

type Shared = Rc<RefCell<Native>>;

fn png(fill: u8) -> String {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend(std::iter::repeat_n(fill, INLINE_MAX));
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn turn(n: u8, status: &str) -> Value {
    let image = png(n);
    let mut items = vec![
        json!({"type": "userMessage", "id": format!("u{n}"), "content": [{"type": "text", "text": "draw"}]}),
        json!({"type": "imageGeneration", "id": format!("ig{n}"), "status": "completed", "revisedPrompt": "p",
               "result": image.clone(), "savedPath": format!("C:/img/{n}.png")}),
        json!({"type": "mcpToolCall", "id": format!("m{n}"), "server": "s", "tool": "t", "status": "completed",
               "result": {"content": [{"type": "image", "mimeType": "image/png", "data": image}]}}),
        json!({"type": "commandExecution", "id": format!("c{n}"), "command": "ls", "status": "completed",
               "aggregatedOutput": "o".repeat(3000)}),
        json!({"type": "agentMessage", "id": format!("a{n}"), "text": format!("done {n}")}),
    ];
    if n == 2 {
        items.push(json!({"type": "commandExecution", "id": "huge", "command": "dump", "status": "completed",
                          "aggregatedOutput": "x".repeat(TEXT_MAX + 5)}));
    }
    json!({"id": format!("turn-{n}"), "status": status, "error": null, "startedAt": n, "completedAt": n,
           "durationMs": 1000, "itemsView": "full", "items": items})
}

fn native(count: u8) -> Shared {
    let shared: Shared = Default::default();
    shared.borrow_mut().turns = (0..count).map(|n| turn(n, "completed")).collect();
    shared
}

fn caller(shared: &Shared) -> impl FnMut(&str, Value) -> Result<Value, HistoryCallError> + '_ {
    move |method, params| {
        let mut native = shared.borrow_mut();
        native.calls.push((method.to_string(), params.clone()));
        match method {
            "thread/read" => {
                let turns = if params["includeTurns"] == true { json!(native.turns) } else { json!([]) };
                Ok(json!({"thread": {"id": "thread-1", "preview": "p", "turns": turns}}))
            }
            "thread/turns/list" => {
                let view = params["itemsView"].as_str().unwrap().to_string();
                if native.no_turns_list {
                    return Err(HistoryCallError::Rpc(json!({"code": -32601, "message": "method not found"})));
                }
                if view == "notLoaded" && native.no_not_loaded {
                    // What serde reports for an unknown enum value: shaped
                    // like the "unknown method" error of older runtimes.
                    return Err(HistoryCallError::Rpc(json!({"code": -32600,
                        "message": "Invalid request: unknown variant `notLoaded`, expected `summary` or `full`"})));
                }
                let limit = params["limit"].as_u64().unwrap() as usize;
                let skip: usize = params["cursor"].as_str().map_or(0, |cursor| cursor.parse().unwrap());
                let total = native.turns.len();
                let data: Vec<Value> = native
                    .turns
                    .iter()
                    .rev()
                    .skip(skip)
                    .take(limit)
                    .map(|turn| {
                        let mut turn = turn.clone();
                        if view == "notLoaded" {
                            turn["items"] = json!([]);
                            turn["itemsView"] = json!("notLoaded");
                        }
                        turn
                    })
                    .collect();
                let next = (skip + data.len() < total).then(|| (skip + data.len()).to_string());
                if view == "notLoaded" {
                    if let Some(turn) = native.append_after_listing.take() {
                        native.turns.push(turn);
                    }
                }
                Ok(json!({"data": data, "nextCursor": next, "backwardsCursor": null}))
            }
            _ => Err(HistoryCallError::Rpc(json!({"code": -32601, "message": "unknown method"}))),
        }
    }
}

fn session(connected: bool) -> Value {
    json!({"native": {"threadId": "thread-1"}, "runtime": {"backend": "host", "appServerConnected": connected, "runtimeGeneration": 3}})
}

struct Setup {
    id: String,
    session: Value,
    store: MediaStore,
    dir: PathBuf,
}

impl Setup {
    fn new(connected: bool) -> Self {
        let dir = std::env::temp_dir().join(format!("cutex-history-turns-{}", uuid::Uuid::new_v4()));
        Self {
            id: format!("cutex.{}", uuid::Uuid::new_v4()),
            session: session(connected),
            store: MediaStore::at(&dir, CACHE_MAX_BYTES),
            dir,
        }
    }

    fn calls<'n>(&self, native: &'n Shared) -> (impl FnMut(&str, Value) -> Result<Value, HistoryCallError> + 'n, bool) {
        (caller(native), self.session["runtime"]["appServerConnected"] == true)
    }

    /// Today's page: the plain read, then `compact` / `media_refs::apply` on the whole body.
    fn old(&self, native: &Shared, query: &HistoryQuery, compact: bool) -> Vec<u8> {
        let (mut call, live) = self.calls(native);
        let mut unused = |_: &str, _: Value| -> Result<Value, HistoryCallError> { panic!("other source") };
        let response = if live {
            respond_read_with(&self.id, &self.session, Some(query), None, &mut call, &mut unused)
        } else {
            respond_read_with(&self.id, &self.session, Some(query), None, &mut unused, &mut call)
        };
        let mut body = response.body;
        assert!(body.get("native").is_some(), "{body}");
        if compact {
            super::super::compact::compact_items(&mut body);
        }
        media_refs::apply(&mut body, &self.id, &self.store);
        serde_json::to_vec(&body).unwrap()
    }

    /// The shaped read; returns the body and (hits, turns read in full).
    fn shaped(&self, native: &Shared, query: &HistoryQuery, compact: bool) -> (Vec<u8>, usize, usize) {
        let mut shaper = TurnShaper::new(&self.id, &self.store, compact);
        let (mut call, live) = self.calls(native);
        let mut unused = |_: &str, _: Value| -> Result<Value, HistoryCallError> { panic!("other source") };
        let response = if live {
            respond_read_shaped_with(&self.id, &self.session, Some(query), None, Some(&mut shaper), &mut call, &mut unused)
        } else {
            respond_read_shaped_with(&self.id, &self.session, Some(query), None, Some(&mut shaper), &mut unused, &mut call)
        };
        let mut body = response.body;
        assert!(body.get("native").is_some(), "{body}");
        shaper.finish_body(&mut body);
        (serde_json::to_vec(&body).unwrap(), shaper.hits, shaper.fetched)
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        invalidate(&self.id);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn page(limit: u32, cursor: Option<&str>) -> HistoryQuery {
    HistoryQuery { limit, cursor: cursor.map(str::to_string) }
}

/// The `thread/turns/list` calls since the last check: (itemsView, limit).
fn lists(native: &Shared) -> Vec<(String, u64)> {
    let calls = std::mem::take(&mut native.borrow_mut().calls);
    calls
        .into_iter()
        .filter(|(method, _)| method == "thread/turns/list")
        .map(|(_, params)| (params["itemsView"].as_str().unwrap().to_string(), params["limit"].as_u64().unwrap()))
        .collect()
}

fn full(limit: u64) -> (String, u64) {
    ("full".to_string(), limit)
}

fn listing(limit: u64) -> (String, u64) {
    ("notLoaded".to_string(), limit)
}

#[test]
fn shaped_pages_are_byte_identical_and_finished_turns_are_read_once() {
    for (connected, compact) in [(true, false), (false, false), (true, true)] {
        let setup = Setup::new(connected);
        let native = native(6);
        let query = page(4, None);
        let expected = setup.old(&native, &query, compact);
        lists(&native);
        // Cold: listed, then the page read in full (exactly today's request).
        let (body, hits, fetched) = setup.shaped(&native, &query, compact);
        assert_eq!(String::from_utf8_lossy(&body), String::from_utf8_lossy(&expected));
        assert_eq!((hits, fetched), (0, 4));
        assert_eq!(lists(&native), [listing(4), full(4)]);
        let refs = if compact { 4 } else { 8 }; // compact cuts the MCP image block first
        assert!(String::from_utf8_lossy(&body).contains(&format!("\"mediaRefs\":{refs}")));
        // Warm: only the live newest turn is read again; offline nothing.
        let (body, hits, fetched) = setup.shaped(&native, &query, compact);
        assert_eq!(body, expected);
        if connected {
            assert_eq!((hits, fetched), (3, 1));
            assert_eq!(lists(&native), [listing(4), full(1)]);
        } else {
            assert_eq!((hits, fetched), (4, 0));
            assert_eq!(lists(&native), [listing(4)]);
        }
        // An older page (native cursor) is cached the same way.
        let older = page(4, Some("4"));
        let expected_older = setup.old(&native, &older, compact);
        lists(&native);
        assert_eq!(setup.shaped(&native, &older, compact).0, expected_older);
        assert_eq!(lists(&native), [listing(4), full(2)]);
        assert_eq!(setup.shaped(&native, &older, compact).0, expected_older);
        assert_eq!(lists(&native), [listing(4)]);
    }
}

#[test]
fn new_or_changed_turns_and_lost_media_are_read_again() {
    let setup = Setup::new(true);
    let native = native(5);
    let query = page(4, None);
    setup.shaped(&native, &query, false);
    // A new turn: it and the previous newest (never cached while live).
    native.borrow_mut().turns.push(turn(5, "inProgress"));
    let expected = setup.old(&native, &query, false);
    lists(&native);
    let (body, _, fetched) = setup.shaped(&native, &query, false);
    assert_eq!(body, expected);
    assert_eq!((fetched, lists(&native)), (2, vec![listing(4), full(2)]));
    // A finished turn whose metadata changed (here: it failed later).
    native.borrow_mut().turns[3]["status"] = json!("failed");
    native.borrow_mut().turns[3]["error"] = json!({"message": "boom"});
    let expected = setup.old(&native, &query, false);
    lists(&native);
    assert_eq!(setup.shaped(&native, &query, false).0, expected);
    assert_eq!(lists(&native), [listing(4), full(3)]);
    // A media file evicted from the cache: its turn is shaped again.
    let (body, _, _) = setup.shaped(&native, &query, false);
    assert_eq!(body, expected);
    let text = String::from_utf8(body).unwrap();
    let sha = text.split("cutex-media:").nth(1).unwrap().split('/').nth(1).unwrap()[..64].to_string();
    std::fs::remove_file(setup.dir.join(format!("{sha}.bin"))).unwrap();
    lists(&native);
    let (body, _, fetched) = setup.shaped(&native, &query, false);
    assert!(fetched >= 2, "the turn holding the evicted image is read again");
    assert!(setup.store.get(&setup.id, &sha).is_some(), "and its media stored again");
    assert_eq!(body, setup.old(&native, &query, false));
}

#[test]
fn invalidation_drops_turns_and_fences_reads_in_flight() {
    let setup = Setup::new(false);
    let native = native(3);
    let query = page(3, None);
    setup.shaped(&native, &query, false);
    assert_eq!(cached_turns(&setup.id), 3);
    super::super::history::invalidate_session(&setup.id);
    assert_eq!(cached_turns(&setup.id), 0);
    // A read that began before an invalidation does not publish.
    let mut shaper = TurnShaper::new(&setup.id, &setup.store, false);
    shaper.begin(&Key::new(&setup.id, "thread-1", "offline", 3), false);
    invalidate(&setup.id);
    shaper.shape(turn(0, "completed"), true);
    assert_eq!(cached_turns(&setup.id), 0);
    setup.shaped(&native, &query, false);
    assert_eq!(cached_turns(&setup.id), 3);
}

#[test]
fn runtimes_without_not_loaded_and_racing_turns_get_the_plain_read() {
    let setup = Setup::new(true);
    let native = native(4);
    native.borrow_mut().no_not_loaded = true;
    let query = page(3, None);
    let expected = setup.old(&native, &query, false);
    lists(&native);
    assert_eq!(setup.shaped(&native, &query, false).0, expected);
    assert_eq!(lists(&native), [listing(3), full(3)]);
    native.borrow_mut().no_not_loaded = false;
    setup.shaped(&native, &query, false);
    lists(&native);
    // A turn starts between the listing and the full read: the page is read
    // whole again, as one plain read after it would have.
    native.borrow_mut().append_after_listing = Some(turn(4, "inProgress"));
    let (body, _, _) = setup.shaped(&native, &query, false);
    assert_eq!(lists(&native), [listing(3), full(1), full(3)]);
    assert_eq!(body, setup.old(&native, &query, false));
}

#[test]
fn runtimes_without_turns_list_keep_the_legacy_read() {
    for connected in [true, false] {
        let setup = Setup::new(connected);
        let native = native(4);
        native.borrow_mut().no_turns_list = true;
        let query = page(3, None);
        let expected = setup.old(&native, &query, false);
        native.borrow_mut().calls.clear();
        assert_eq!(setup.shaped(&native, &query, false).0, expected);
        let calls = std::mem::take(&mut native.borrow_mut().calls);
        assert!(calls.iter().any(|(method, params)| method == "thread/read" && params["includeTurns"] == true));
    }
}

#[test]
fn a_live_newest_turn_is_cached_once_it_has_settled() {
    TEST_SETTLE.with(|settle| settle.set(std::time::Duration::from_millis(50)));
    let setup = Setup::new(true);
    let native = native(4);
    let query = page(3, None);
    let expected = setup.old(&native, &query, false);
    lists(&native);
    // First seen final: not cached yet.
    assert_eq!(setup.shaped(&native, &query, false), (expected.clone(), 0, 3));
    assert_eq!(cached_turns(&setup.id), 2);
    std::thread::sleep(std::time::Duration::from_millis(80));
    // Settled: read once more and kept; then nothing is read in full.
    assert_eq!(setup.shaped(&native, &query, false), (expected.clone(), 2, 1));
    assert_eq!(setup.shaped(&native, &query, false), (expected, 3, 0));
    lists(&native);
    // Its metadata changes (it was only listed interrupted): read again and
    // settling starts over.
    native.borrow_mut().turns[3]["status"] = json!("interrupted");
    let expected = setup.old(&native, &query, false);
    lists(&native);
    assert_eq!(setup.shaped(&native, &query, false), (expected.clone(), 2, 1));
    assert_eq!(setup.shaped(&native, &query, false), (expected.clone(), 2, 1));
    std::thread::sleep(std::time::Duration::from_millis(80));
    assert_eq!(setup.shaped(&native, &query, false), (expected.clone(), 2, 1));
    assert_eq!(setup.shaped(&native, &query, false), (expected, 3, 0));
    // A running newest turn never settles.
    native.borrow_mut().turns.push(turn(4, "inProgress"));
    let expected = setup.old(&native, &query, false);
    std::thread::sleep(std::time::Duration::from_millis(80));
    assert_eq!(setup.shaped(&native, &query, false), (expected.clone(), 2, 1));
    std::thread::sleep(std::time::Duration::from_millis(80));
    assert_eq!(setup.shaped(&native, &query, false), (expected, 2, 1));
}

#[test]
fn saved_file_turn_cache_reuses_missing_targets_and_fences_runtime_gate_changes() {
    let setup = Setup::new(false);
    let key = Key::new(&setup.id, "saved-thread", "offline", 3);
    let missing = setup.dir.join("missing-generated.png");
    let input = json!({"id":"saved-turn", "status":"completed", "items":[
        {"id":"image", "type":"imageGeneration", "result":"", "savedPath":missing}
    ]});
    let mut shaper = TurnShaper::new(&setup.id, &setup.store, false).with_saved_files(true);
    shaper.begin(&key, false);
    let shaped = shaper.shape(input.clone(), true);
    assert!(shaped["items"][0]["result"].as_str().unwrap().starts_with("cutex-media:"));
    assert_eq!(shaper.hit(&input, true).unwrap().0, shaped);
    assert!(!missing.exists());
    let mut older = TurnShaper::new(&setup.id, &setup.store, false);
    older.begin(&key, false);
    assert!(older.hit(&input, true).is_none(), "r54/unknown runtime cannot reuse r55 file refs");
    let reference = shaped["items"][0]["result"].as_str().unwrap();
    let sha = reference.rsplit('/').next().unwrap();
    std::fs::remove_file(setup.dir.join(format!("{sha}.json"))).unwrap();
    assert!(shaper.hit(&input, true).is_none(), "evicted descriptors cause an ordinary page refetch");
}
