//! Bounded, process-local display-history index. No native I/O under the cache lock.
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{json, Value};

use super::history::HistoryCallError;

const CAPACITY: usize = 64;
const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_TURNS: usize = 100_000;
const PAGE_SIZE: usize = 100; // Codex caps at 100; Claude caps at 200.
const TAIL_SIZE: usize = 16;

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct Key {
    session: String,
    thread: String,
    source: String,
    generation: u64,
}

impl Key {
    pub fn new(session: &str, thread: &str, source: &str, generation: u64) -> Self {
        Self {
            session: session.into(),
            thread: thread.into(),
            source: source.into(),
            generation,
        }
    }

    pub(super) fn matches_thread(&self, thread: &str) -> bool {
        self.thread == thread
    }
}

#[derive(Debug)]
pub struct Index {
    /// Native chronological order; metadata only, never cached item contents.
    pub turns: Vec<Value>,
    positions: HashMap<String, usize>,
    bytes: usize,
    pub legacy: bool,
}

impl Index {
    fn new(mut turns: Vec<Value>, legacy: bool) -> Result<Self, HistoryCallError> {
        let mut positions = HashMap::new();
        let mut bytes = 0;
        for (i, turn) in turns.iter_mut().enumerate() {
            let id = turn
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && id.len() <= 4096)
                .ok_or_else(|| invalid("history turn has no valid id"))?
                .to_owned();
            if positions.insert(id, i).is_some() {
                return Err(invalid("duplicate history turn id"));
            }
            turn["items"] = json!([]);
            turn["itemsView"] = json!("notLoaded");
            bytes += turn.to_string().len();
            if bytes > MAX_BYTES || i >= MAX_TURNS {
                return Err(invalid("history index exceeds its capacity"));
            }
        }
        Ok(Self {
            turns,
            positions,
            bytes,
            legacy,
        })
    }

    pub fn ordinal(&self, id: &str) -> Option<usize> {
        self.positions.get(id).map(|i| i + 1)
    }

    pub fn ordinals(&self, turns: &[Value]) -> (Option<usize>, Option<usize>) {
        if turns.is_empty() {
            return (None, Some(self.turns.len()));
        }
        let Some(first) = turns[0]
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| self.ordinal(id))
        else {
            return (None, None);
        };
        if turns.iter().enumerate().all(|(i, turn)| {
            turn.get("id")
                .and_then(Value::as_str)
                .and_then(|id| self.ordinal(id))
                == Some(first + i)
        }) {
            (Some(first), Some(self.turns.len()))
        } else {
            (None, None)
        }
    }
}

#[derive(Default)]
struct Cache {
    entries: HashMap<Key, Entry>,
    clock: u64,
}

struct Entry {
    index: Option<Arc<Index>>,
    used: u64,
    token: Arc<()>,
}

impl Cache {
    fn evict(&mut self) {
        while self.entries.len() > CAPACITY
            || self
                .entries
                .values()
                .map(|e| e.index.as_ref().map_or(0, |v| v.bytes))
                .sum::<usize>()
                > MAX_BYTES
        {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| key.clone())
                .unwrap();
            self.entries.remove(&oldest);
        }
    }
}

static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
fn cache() -> std::sync::MutexGuard<'static, Cache> {
    CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

pub fn invalidate(session: &str) {
    let mut cache = cache();
    // Removing the token also fences in-flight pre-mutation reads.
    cache.entries.retain(|key, _| key.session != session);
}

fn get(key: &Key) -> (Option<Arc<Index>>, Arc<()>) {
    let mut cache = cache();
    // Rebinding (including a new runtime generation) drops the previous binding.
    cache.entries.retain(|other, _| {
        other.session != key.session
            || (other.thread == key.thread && other.generation == key.generation)
    });
    cache.clock += 1;
    let clock = cache.clock;
    let entry = cache.entries.entry(key.clone()).or_insert_with(|| Entry {
        index: None,
        used: clock,
        token: Arc::new(()),
    });
    entry.used = clock;
    let result = (entry.index.clone(), entry.token.clone());
    cache.evict();
    result
}

/// The cached index for `key`, if any; never calls native.
pub fn cached(key: &Key) -> Option<Arc<Index>> {
    get(key).0
}

fn put(key: Key, index: Index, token: Arc<()>) -> Arc<Index> {
    let index = Arc::new(index);
    let mut cache = cache();
    if !cache
        .entries
        .get(&key)
        .is_some_and(|entry| Arc::ptr_eq(&entry.token, &token))
    {
        return index;
    }
    cache.clock += 1;
    let clock = cache.clock;
    cache.entries.insert(
        key,
        Entry {
            index: Some(index.clone()),
            used: clock,
            token,
        },
    );
    cache.evict();
    index
}

pub(super) fn invalid(message: &str) -> HistoryCallError {
    HistoryCallError::Rpc(json!({ "code": -32603, "message": message }))
}

pub(super) fn unsupported(error: &HistoryCallError) -> bool {
    match error {
        HistoryCallError::Rpc(error) => {
            error["code"] == -32601
                || (error["code"] == -32600
                    && error["message"]
                        .as_str()
                        .unwrap_or_default()
                        .to_lowercase()
                        .contains("unknown"))
        }
        _ => false,
    }
}

pub(super) fn page_data(page: &Value) -> Result<Vec<Value>, HistoryCallError> {
    page.get("data")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| invalid("history page has no data"))
}

pub(super) fn next_cursor(page: &Value) -> Result<Option<String>, HistoryCallError> {
    match page.get("nextCursor") {
        Some(Value::Null) => Ok(None),
        Some(Value::String(cursor)) if !cursor.is_empty() && cursor.len() <= 4096 => {
            Ok(Some(cursor.clone()))
        }
        _ => Err(invalid("history page has no valid nextCursor")),
    }
}

/// A whole newest page can reconcile both append and tail revert, provided all
/// overlapping IDs still have identical relative order. No overlap => rebuild.
fn reconcile(old: &Index, newest: &[Value], more: bool) -> Result<Option<Index>, HistoryCallError> {
    let tail = Index::new(newest.to_vec(), false)?;
    if !more {
        return Ok(Some(tail));
    }
    let anchor = newest.iter().enumerate().find_map(|(i, t)| {
        t["id"]
            .as_str()
            .and_then(|id| old.ordinal(id))
            .map(|n| (i, n - 1))
    });
    let Some((i, position)) = anchor else {
        return Ok(None);
    };
    if i != 0 {
        return Ok(None);
    }
    for (i, turn) in newest.iter().enumerate() {
        if let Some(n) = old.ordinal(turn["id"].as_str().unwrap()) {
            if n - 1 != position + i {
                return Ok(None);
            }
        } else if position + i < old.turns.len() {
            // Revert followed by new turns replaces the old suffix.
            if newest[i..]
                .iter()
                .any(|t| old.ordinal(t["id"].as_str().unwrap()).is_some())
            {
                return Ok(None);
            }
            break;
        }
    }
    let mut turns = old.turns[..position].to_vec();
    turns.extend_from_slice(newest);
    Ok(Some(Index::new(turns, false)?))
}

/// Bootstrap never adds native calls: cold histories longer than the returned
/// page (at most 200 turns) report unknown until /history or single-turn read
/// builds the index. This also avoids unbounded background native work.
pub fn observe_newest(
    key: &Key,
    newest: &[Value],
    more: bool,
) -> Result<Option<Arc<Index>>, HistoryCallError> {
    let (old, epoch) = get(key);
    let index = if !more {
        Some(Index::new(newest.to_vec(), false)?)
    } else if let Some(old) = old {
        reconcile(&old, newest, more)?
    } else {
        None
    };
    Ok(index.map(|index| put(key.clone(), index, epoch)))
}

pub fn read(
    key: &Key,
    call: &mut dyn FnMut(&str, Value) -> Result<Value, HistoryCallError>,
) -> Result<Arc<Index>, HistoryCallError> {
    let (old, epoch) = get(key);
    let mut page = match call(
        "thread/turns/list",
        json!({
            "threadId": key.thread, "cursor": null, "limit": if old.is_some() { TAIL_SIZE } else { PAGE_SIZE },
            "sortDirection": "desc", "itemsView": "notLoaded",
        }),
    ) {
        Ok(page) => page,
        Err(error) if unsupported(&error) => {
            let full = call(
                "thread/read",
                json!({"threadId": key.thread, "includeTurns": true}),
            )?;
            if full.pointer("/thread/id").and_then(Value::as_str) != Some(key.thread.as_str())
                || full.pointer("/thread/historyMode").and_then(Value::as_str) == Some("paginated")
            {
                return Err(invalid("legacy full history is not complete"));
            }
            let turns = full
                .pointer("/thread/turns")
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| invalid("legacy history has no turns"))?;
            return Ok(put(key.clone(), Index::new(turns, true)?, epoch));
        }
        Err(error) => return Err(error),
    };
    let mut descending = page_data(&page)?;
    if let Some(old) = old {
        let mut newest = descending.clone();
        newest.reverse();
        if let Some(index) = reconcile(&old, &newest, next_cursor(&page)?.is_some())? {
            return Ok(put(key.clone(), index, epoch));
        }
    }
    let mut cursors = HashSet::new();
    let mut seen = HashSet::new();
    let mut bytes = 0;
    loop {
        for turn in page_data(&page)? {
            let id = turn["id"]
                .as_str()
                .ok_or_else(|| invalid("history turn has no id"))?
                .to_owned();
            if !seen.insert(id) {
                return Err(invalid("duplicate history turn id"));
            }
            bytes += turn.to_string().len();
        }
        if bytes > MAX_BYTES || descending.len() > MAX_TURNS {
            return Err(invalid("history index exceeds its capacity"));
        }
        let Some(cursor) = next_cursor(&page)? else {
            break;
        };
        if !cursors.insert(cursor.clone()) || page_data(&page)?.is_empty() {
            return Err(invalid("history cursor did not advance"));
        }
        page = call(
            "thread/turns/list",
            json!({"threadId": key.thread, "cursor": cursor,
            "limit": PAGE_SIZE, "sortDirection": "desc", "itemsView": "notLoaded"}),
        )?;
        descending.extend(page_data(&page)?);
    }
    descending.reverse();
    Ok(put(key.clone(), Index::new(descending, false)?, epoch))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> Key {
        Key::new(&uuid::Uuid::new_v4().to_string(), "thread", "live", 1)
    }
    fn turns(count: usize) -> Vec<Value> {
        (0..count)
            .map(|n| json!({"id": format!("t{n}"), "status": "completed"}))
            .collect()
    }

    fn native<'a>(
        turns: &'a [Value],
        calls: &'a mut usize,
    ) -> impl FnMut(&str, Value) -> Result<Value, HistoryCallError> + 'a {
        move |method, params| {
            *calls += 1;
            assert_eq!(method, "thread/turns/list");
            assert_eq!(params["itemsView"], "notLoaded");
            let offset = params["cursor"]
                .as_str()
                .map(|c| c.parse::<usize>().unwrap())
                .unwrap_or(0);
            let count = (params["limit"].as_u64().unwrap() as usize).min(100);
            let data: Vec<_> = turns
                .iter()
                .rev()
                .skip(offset)
                .take(count)
                .cloned()
                .collect();
            Ok(
                json!({"nextCursor": (offset + data.len() < turns.len()).then(|| (offset + data.len()).to_string()), "data": data}),
            )
        }
    }

    #[test]
    fn cold_append_revert_reappend_rebind_and_source_isolation() {
        let key = key();
        let mut calls = 0;
        let index = read(&key, &mut native(&turns(1500), &mut calls)).unwrap();
        assert_eq!(calls, 15);
        assert_eq!(index.ordinal("t123"), Some(124));
        calls = 0;
        let index = read(&key, &mut native(&turns(1503), &mut calls)).unwrap();
        assert_eq!(calls, 1);
        assert_eq!(index.turns.len(), 1503);
        calls = 0;
        let index = read(&key, &mut native(&turns(1400), &mut calls)).unwrap();
        assert_eq!(calls, 1);
        assert_eq!(index.ordinal("t1400"), None);
        let mut replacement = turns(1398);
        replacement.push(json!({"id": "replacement"}));
        let index = read(&key, &mut native(&replacement, &mut calls)).unwrap();
        assert_eq!(index.ordinal("replacement"), Some(1399));
        let offline = Key {
            source: "offline".into(),
            ..key.clone()
        };
        calls = 0;
        read(&offline, &mut native(&turns(1300), &mut calls)).unwrap();
        assert_eq!(calls, 13);
        let rebound = Key {
            thread: "rebound".into(),
            ..key.clone()
        };
        calls = 0;
        read(&rebound, &mut native(&turns(1200), &mut calls)).unwrap();
        assert_eq!(calls, 12);
        assert!(get(&key).0.is_none());
        invalidate(&key.session);
        assert!(get(&rebound).0.is_none());
    }

    #[test]
    fn discontinuous_tail_rebuilds_and_fork_numbers_its_own_prefix() {
        let key = key();
        let mut calls = 0;
        read(&key, &mut native(&turns(100), &mut calls)).unwrap();
        calls = 0;
        let index = read(&key, &mut native(&turns(1200), &mut calls)).unwrap();
        assert_eq!(calls, 13);
        assert_eq!(index.turns.len(), 1200);
        let fork = Key {
            session: "fork-test".into(),
            thread: "fork".into(),
            ..key
        };
        let index = read(&fork, &mut native(&turns(36), &mut calls)).unwrap();
        assert_eq!(index.ordinal("t35"), Some(36));
        assert_eq!(index.turns.len(), 36);
    }

    #[test]
    fn duplicate_ids_and_stuck_or_empty_cursors_are_errors() {
        for mode in 0..5 {
            let mut call_count = 0;
            let result = read(&key(), &mut |_, _| {
                call_count += 1;
                Ok(match mode {
                    0 => json!({"data": [{"id":"same"}, {"id":"same"}], "nextCursor":null}),
                    1 => json!({"data": [{"id":format!("t{call_count}")}], "nextCursor":"stuck"}),
                    2 => json!({"data": [], "nextCursor":"stuck"}),
                    3 => json!({"data": [{"id":"one"}]}),
                    _ => json!({"data": [{"id":"one"}], "nextCursor":42}),
                })
            });
            assert!(result.is_err());
            assert!(call_count <= 2);
        }
    }

    #[test]
    fn legacy_fallback_and_incomplete_legacy_rejection() {
        for paginated in [false, true] {
            let result = read(&key(), &mut |method, _| {
                if method == "thread/turns/list" {
                    Err(HistoryCallError::Rpc(json!({"code":-32601})))
                } else {
                    Ok(
                        json!({"thread":{"id":"thread", "turns":turns(12), "historyMode":if paginated {"paginated"} else {"legacy"}}}),
                    )
                }
            });
            if paginated {
                assert!(result.is_err());
            } else {
                let index = result.unwrap();
                assert!(index.legacy);
                assert_eq!(index.ordinal("t11"), Some(12));
            }
        }
    }

    #[test]
    fn bootstrap_cold_unknown_warm_known_and_empty_page() {
        let key = key();
        let all = turns(1500);
        assert!(observe_newest(&key, &all[1480..], true).unwrap().is_none());
        let mut calls = 0;
        read(&key, &mut native(&all, &mut calls)).unwrap();
        let index = observe_newest(&key, &all[1480..], true).unwrap().unwrap();
        assert_eq!(index.ordinals(&all[1480..]), (Some(1481), Some(1500)));
        assert_eq!(index.ordinals(&[]), (None, Some(1500)));
        assert_eq!(
            index.ordinals(&[all[0].clone(), all[2].clone()]),
            (None, None)
        );
        let index = observe_newest(&key, &[], false).unwrap().unwrap();
        assert_eq!(index.ordinals(&[]), (None, Some(0)));
    }

    #[test]
    fn cache_is_bounded_and_invalidation_fences_inflight_publish() {
        let key = key();
        let epoch = get(&key).1;
        invalidate(&key.session);
        put(key.clone(), Index::new(turns(2), false).unwrap(), epoch);
        assert!(get(&key).0.is_none());
        let other = self::key();
        let token = get(&other).1;
        invalidate(&key.session);
        put(other.clone(), Index::new(turns(2), false).unwrap(), token);
        assert!(
            get(&other).0.is_some(),
            "unrelated invalidation must not fence this build"
        );
        let mut local = Cache::default();
        for n in 0..70 {
            local.entries.insert(
                self::key(),
                Entry {
                    index: Some(Arc::new(Index::new(turns(2), false).unwrap())),
                    used: n,
                    token: Arc::new(()),
                },
            );
            local.evict();
        }
        assert_eq!(local.entries.len(), CAPACITY);
    }
}
