//! Shaped finished turns for `media=ref` history reads (r252).
//!
//! An image-generating agent's newest 30 turns were ~393 MB of native
//! `thread/turns/list itemsView=full` JSON (generated images inline as
//! base64), copied and hashed on every open only to become 2.35 MB of
//! references. With `media=ref`, `/history` pages and the bootstrap's
//! `turnLimit` page read the page with `itemsView: "notLoaded"` first (turn
//! metadata only) and fetch `itemsView: "full"` only for the newest turns
//! down to the oldest one not cached here; every turn read in full is shaped
//! (`compact`, then `media_refs` references and text caps, exactly what the
//! whole-body pass does to it) and a finished one is kept here, per session
//! binding (`turn_index::Key`: thread, source, runtime generation) and
//! `compact` choice.
//!
//! A cached turn is used only when the listed turn still has the same
//! metadata (everything but `items` / `itemsView`: id, status, error,
//! timestamps, duration), its status is final, and every media file it
//! references is still in the media cache (touched as a fresh read would).
//! The newest turn of a live thread is cached only after it has been listed
//! final with unchanged metadata for `SETTLE` (right after it ends, the
//! runtime's persisted history may still lag its in-memory snapshot); a
//! running turn never is. `history::invalidate_session` (revert,
//! fork) drops a session's turns and fences reads in flight; a rollback or
//! compaction that drops turns simply stops listing them. The response is
//! the same bytes as the uncached read: `mediaRefs` counts cached turns'
//! references too.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{Map, Value};

use super::media_refs::{MediaStore, Replacer, TEXT_MAX};
use super::turn_index::Key;

/// Bound on all cached shaped turns (serialized bytes).
const MAX_BYTES: usize = 64 * 1024 * 1024;
/// A larger shaped turn is not cached.
const MAX_TURN_BYTES: usize = 8 * 1024 * 1024;
/// How long a live thread's newest turn must be listed final, unchanged,
/// before it is cached.
const SETTLE: std::time::Duration = std::time::Duration::from_secs(10);
/// Bound on `Cache::settling` entries.
const MAX_SETTLING: usize = 1024;

#[cfg(not(test))]
fn settle() -> std::time::Duration {
    SETTLE
}

#[cfg(test)]
thread_local! {
    /// Tests run on their own threads; one may shorten `SETTLE` for itself.
    pub(super) static TEST_SETTLE: std::cell::Cell<std::time::Duration> = const { std::cell::Cell::new(SETTLE) };
}

#[cfg(test)]
fn settle() -> std::time::Duration {
    TEST_SETTLE.with(std::cell::Cell::get)
}
/// Where `/history` and the bootstrap put a page's turns.
pub const TURNS_POINTER: &str = "/native/result/thread/turns";

/// Applies `compact` / `media=ref` to a history page while it is read and
/// caches the shaped finished turns. Build one per request.
pub struct TurnShaper<'a> {
    store: &'a MediaStore,
    session: &'a str,
    compact: bool,
    saved_files: bool,
    scope: Option<Scope>,
    /// `mediaRefs` of the page's shaped turns.
    refs: usize,
    shaped: bool,
    /// Turns served from the cache / read in full by the last page read.
    pub hits: usize,
    pub fetched: usize,
}

#[derive(Clone)]
struct Scope {
    key: Key,
    live: bool,
    fence: u64,
}

struct CachedTurn {
    saved_files: bool,
    meta: Map<String, Value>,
    turn: Value,
    media: Vec<(String, String)>,
    refs: usize,
    bytes: usize,
}

impl<'a> TurnShaper<'a> {
    pub fn new(session: &'a str, store: &'a MediaStore, compact: bool) -> Self {
        Self { store, session, compact, saved_files: false, scope: None, refs: 0, shaped: false, hits: 0, fetched: 0 }
    }

    pub fn with_saved_files(mut self, enabled: bool) -> Self {
        self.saved_files = enabled;
        self
    }

    /// Start a page read for `key` (`live`: the runtime's own reader).
    pub(super) fn begin(&mut self, key: &Key, live: bool) {
        let fence = cache().fences.get(self.session).copied().unwrap_or(0);
        self.scope = Some(Scope { key: key.clone(), live, fence });
        self.refs = 0;
        self.shaped = false;
        self.hits = 0;
        self.fetched = 0;
    }

    pub(super) fn caching(&self) -> bool {
        self.scope.is_some()
    }

    /// Whether the turn at `index` (newest first) of a page may be cached.
    pub(super) fn cacheable(&self, turn: &Value, index: usize, first_page: bool) -> bool {
        let Some(scope) = &self.scope else {
            return false;
        };
        if !matches!(turn.get("status").and_then(Value::as_str), Some("completed" | "interrupted" | "failed")) {
            return false;
        }
        if !(scope.live && first_page && index == 0) {
            return true;
        }
        // The live thread's newest turn: only once it has been listed final
        // with the same metadata for `SETTLE` (its persisted history may lag
        // the runtime's in-memory snapshot right after it ends).
        let Some(id) = turn.get("id").and_then(Value::as_str) else {
            return false;
        };
        let meta = meta(turn);
        let now = std::time::Instant::now();
        let mut cache = cache();
        match cache.settling.get(&scope.key) {
            Some(seen) if seen.id == id && seen.meta == meta => now.duration_since(seen.since) >= settle(),
            _ => {
                if cache.settling.len() >= MAX_SETTLING {
                    cache.settling.clear();
                }
                let seen = Settling { session: self.session.to_string(), id: id.to_string(), meta, since: now };
                cache.settling.insert(scope.key.clone(), seen);
                false
            }
        }
    }

    /// The cached shaped turn for a listed turn, when still valid.
    pub(super) fn hit(&self, listed: &Value, cacheable: bool) -> Option<(Value, usize)> {
        let scope = self.scope.as_ref().filter(|_| cacheable)?;
        let id = listed.get("id").and_then(Value::as_str)?;
        let cached = cache().turns.get_mut(&(scope.key.clone(), self.compact, id.to_string())).map(|slot| {
            slot.used = next_clock();
            slot.turn.clone()
        })?;
        if cached.saved_files != self.saved_files || cached.meta != meta(listed) {
            return None;
        }
        if !cached.media.iter().all(|(sha, mime)| self.store.reuse(sha, mime, self.session)) {
            return None;
        }
        Some((cached.turn.clone(), cached.refs))
    }

    /// Count a cached turn's references into this page's `mediaRefs`.
    pub(super) fn used(&mut self, refs: usize) {
        self.refs += refs;
        self.hits += 1;
    }

    /// Shape a turn read in full (what `compact_items` and
    /// `media_refs::apply` do to it inside a whole body) and cache it when
    /// `cacheable`.
    pub(super) fn shape(&mut self, mut turn: Value, cacheable: bool) -> Value {
        self.fetched += 1;
        let meta = cacheable.then(|| meta(&turn));
        if self.compact {
            super::compact::compact_items(&mut turn);
        }
        let mut replacer = Replacer::new(self.store, self.session).with_saved_files(self.saved_files);
        replacer.replace_saved_turn(&mut turn);
        replacer.replace(&mut turn);
        super::compact::cap_tool_strings(&mut turn, TEXT_MAX);
        self.refs += replacer.replaced;
        let id = turn.get("id").and_then(Value::as_str).map(str::to_string);
        if let (Some(meta), Some(scope), Some(id), 0) = (meta, &self.scope, id, replacer.failed) {
            let bytes = serde_json::to_vec(&turn).map_or(usize::MAX, |bytes| bytes.len());
            if bytes <= MAX_TURN_BYTES {
                put(
                    self.session,
                    scope,
                    self.compact,
                    id,
                    CachedTurn { saved_files: self.saved_files, meta, turn: turn.clone(), media: replacer.recorded, refs: replacer.replaced, bytes },
                );
            }
        }
        turn
    }

    pub(super) fn finished(&mut self) {
        self.shaped = true;
    }

    /// Finish a success body: `compact` / `media=ref` for everything but
    /// the turns already shaped while reading (all of it when the page was
    /// not read through this shaper), then `mediaRefs` for the whole body.
    pub fn finish_body(&self, body: &mut Value) {
        let turns = if self.shaped { body.pointer_mut(TURNS_POINTER).map(Value::take) } else { None };
        if self.compact {
            super::compact::compact_items(body);
        }
        super::media_refs::apply_with_saved_files(body, self.session, self.store, self.saved_files);
        if let Some(turns) = turns {
            if let Some(slot) = body.pointer_mut(TURNS_POINTER) {
                *slot = turns;
            }
            let rest = body.get("mediaRefs").and_then(Value::as_u64).unwrap_or(0) as usize;
            if let Some(map) = body.as_object_mut() {
                map.insert("mediaRefs".to_string(), Value::from(rest + self.refs));
            }
        }
    }
}

/// A turn without `items` / `itemsView`.
fn meta(turn: &Value) -> Map<String, Value> {
    turn.as_object()
        .map(|map| {
            map.iter()
                .filter(|(key, _)| !matches!(key.as_str(), "items" | "itemsView"))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default()
}

struct Slot {
    session: String,
    turn: Arc<CachedTurn>,
    used: u64,
}

#[derive(Default)]
struct Cache {
    turns: HashMap<(Key, bool, String), Slot>,
    /// Bumped by `invalidate`; a read that began before it does not publish.
    fences: HashMap<String, u64>,
    bytes: usize,
    /// Per live binding: its newest finished turn and since when it is
    /// listed so (`TurnShaper::cacheable`).
    settling: HashMap<Key, Settling>,
}

struct Settling {
    session: String,
    id: String,
    meta: Map<String, Value>,
    since: std::time::Instant,
}

static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
static CLOCK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn cache() -> std::sync::MutexGuard<'static, Cache> {
    CACHE.get_or_init(Default::default).lock().unwrap_or_else(|error| error.into_inner())
}

fn next_clock() -> u64 {
    CLOCK.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
}

fn put(session: &str, scope: &Scope, compact: bool, id: String, turn: CachedTurn) {
    let mut cache = cache();
    if cache.fences.get(session).copied().unwrap_or(0) != scope.fence {
        return;
    }
    cache.bytes += turn.bytes;
    let slot = Slot { session: session.to_string(), turn: Arc::new(turn), used: next_clock() };
    if let Some(old) = cache.turns.insert((scope.key.clone(), compact, id), slot) {
        cache.bytes -= old.turn.bytes;
    }
    while cache.bytes > MAX_BYTES {
        let Some(oldest) = cache.turns.iter().min_by_key(|(_, slot)| slot.used).map(|(key, _)| key.clone()) else {
            break;
        };
        if let Some(old) = cache.turns.remove(&oldest) {
            cache.bytes -= old.turn.bytes;
        }
    }
}

/// Drop a session's shaped turns (history revert, fork) and fence the
/// reads in flight.
pub fn invalidate(session: &str) {
    let mut cache = cache();
    *cache.fences.entry(session.to_string()).or_insert(0) += 1;
    let mut freed = 0;
    cache.turns.retain(|_, slot| {
        let keep = slot.session != session;
        if !keep {
            freed += slot.turn.bytes;
        }
        keep
    });
    cache.bytes -= freed;
    cache.settling.retain(|_, seen| seen.session != session);
}

#[cfg(test)]
pub(super) fn cached_turns(session: &str) -> usize {
    cache().turns.values().filter(|slot| slot.session == session).count()
}

#[cfg(test)]
#[path = "history_turns_tests.rs"]
mod tests;
