//! Sub-agent thread lineage for a cutex session.
//!
//! A cutex session is bound to one native app-server thread (its root). When
//! that agent spawns Codex sub-agents (multi-agent v1 `collabAgentToolCall`
//! spawns, v2 `subAgentActivity`), each sub-agent is a separate native thread
//! in the same app-server whose `parentThreadId` (and
//! `source.subAgent.thread_spawn.parent_thread_id`) names the spawning thread.
//!
//! This module keeps, per cutex session, the set of threads verified to be
//! descendants of the root, so Management can:
//! - allow read-only native methods on descendant threads, and
//! - publish descendant live notifications onto the parent session's stream.
//!
//! Lineage is learned only from the app-server itself: from `thread/read`
//! responses (walking `parentThreadId` up to the root) and from spawn items
//! emitted on an already-trusted thread. Notifications for a thread whose
//! lineage is still unknown are buffered briefly (bounded) until it resolves.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use serde_json::Value;

use crate::app_server::client::AppServerEvent;

/// Native read-only methods that may target a verified descendant thread.
pub const SUBAGENT_READ_METHODS: &[&str] = &[
    "thread/read",
    "thread/turns/list",
    "thread/items/list",
    "thread/backgroundTerminals/list",
];

/// Maximum ancestry walk (Codex caps spawn depth far below this).
pub const MAX_LINEAGE_DEPTH: usize = 16;
const MAX_BUFFERED_EVENTS: usize = 512;
const BUFFER_TTL: Duration = Duration::from_secs(120);
const RESOLVE_RETRY: Duration = Duration::from_secs(5);

#[derive(Default)]
struct PendingThread {
    first_seen: Option<Instant>,
    resolve_started: Option<Instant>,
}

#[derive(Default)]
struct SessionLineage {
    root: String,
    /// Verified child -> parent edges; every chain ends at `root`.
    parents: HashMap<String, String>,
    /// Threads whose ancestry was read and does not reach `root`.
    rejected: HashSet<String>,
    pending: HashMap<String, PendingThread>,
    /// Buffered events of unresolved threads, keyed by arrival sequence.
    buffered: BTreeMap<u64, (String, AppServerEvent)>,
    next_sequence: u64,
}

impl SessionLineage {
    fn is_descendant(&self, thread_id: &str) -> bool {
        let mut current = thread_id;
        for _ in 0..MAX_LINEAGE_DEPTH {
            match self.parents.get(current) {
                Some(parent) if parent == &self.root => return true,
                Some(parent) => current = parent,
                None => return false,
            }
        }
        false
    }

    fn is_trusted(&self, thread_id: &str) -> bool {
        thread_id == self.root || self.is_descendant(thread_id)
    }

    fn record_edge(&mut self, child: &str, parent: &str) -> bool {
        if child.is_empty() || child == self.root || child == parent || !self.is_trusted(parent) {
            return false;
        }
        if self.parents.contains_key(child) {
            return self.is_descendant(child);
        }
        // Refuse an edge that would make `child` its own ancestor.
        let mut current = parent;
        for _ in 0..MAX_LINEAGE_DEPTH {
            if current == child {
                return false;
            }
            match self.parents.get(current) {
                Some(next) => current = next,
                None => break,
            }
        }
        self.parents.insert(child.to_string(), parent.to_string());
        self.rejected.remove(child);
        true
    }
}

static LINEAGES: OnceLock<Mutex<HashMap<String, SessionLineage>>> = OnceLock::new();

fn with_session<R>(
    cutex_session_id: &str,
    root_thread_id: &str,
    action: impl FnOnce(&mut SessionLineage) -> R,
) -> R {
    let lineages = LINEAGES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut lineages = lineages
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let entry = lineages
        .entry(cutex_session_id.to_string())
        .or_default();
    if entry.root != root_thread_id {
        *entry = SessionLineage {
            root: root_thread_id.to_string(),
            ..SessionLineage::default()
        };
    }
    action(entry)
}

/// Whether `thread_id` is a verified (transitive) sub-agent of the root.
pub fn is_descendant(cutex_session_id: &str, root_thread_id: &str, thread_id: &str) -> bool {
    with_session(cutex_session_id, root_thread_id, |lineage| {
        lineage.is_descendant(thread_id)
    })
}

/// Records `child -> parent` when `parent` is the root or a known descendant.
pub fn record_edge(
    cutex_session_id: &str,
    root_thread_id: &str,
    child_thread_id: &str,
    parent_thread_id: &str,
) -> bool {
    with_session(cutex_session_id, root_thread_id, |lineage| {
        lineage.record_edge(child_thread_id, parent_thread_id)
    })
}

/// The spawning thread of a native `Thread` object, if it is a sub-agent.
pub fn parent_thread_id(thread: &Value) -> Option<String> {
    thread
        .get("parentThreadId")
        .and_then(Value::as_str)
        .or_else(|| {
            thread
                .pointer("/source/subAgent/thread_spawn/parent_thread_id")
                .and_then(Value::as_str)
        })
        .filter(|parent| !parent.is_empty())
        .map(str::to_string)
}

/// Lineage reader failure classes for [`verify_descendant`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineageReadError {
    /// The app-server could not be asked; the answer is unknown.
    Unavailable(String),
}

/// Verifies that `thread_id` descends from the root by walking
/// `parentThreadId` through `read_thread` (which returns the native `Thread`
/// object, or `None` when the app-server does not know the thread). Verified
/// edges are cached; a definite negative answer marks the thread rejected.
pub fn verify_descendant(
    cutex_session_id: &str,
    root_thread_id: &str,
    thread_id: &str,
    mut read_thread: impl FnMut(&str) -> Result<Option<Value>, LineageReadError>,
) -> Result<bool, LineageReadError> {
    if thread_id.is_empty() || thread_id == root_thread_id {
        return Ok(false);
    }
    let mut chain: Vec<(String, String)> = Vec::new();
    let mut current = thread_id.to_string();
    for _ in 0..MAX_LINEAGE_DEPTH {
        if current == root_thread_id || is_descendant(cutex_session_id, root_thread_id, &current) {
            let recorded = with_session(cutex_session_id, root_thread_id, |lineage| {
                for (child, parent) in chain.iter().rev() {
                    if !lineage.record_edge(child, parent) {
                        return false;
                    }
                }
                true
            });
            return Ok(recorded);
        }
        let Some(thread) = read_thread(&current)? else {
            break;
        };
        if thread.get("id").and_then(Value::as_str) != Some(current.as_str()) {
            break;
        }
        let Some(parent) = parent_thread_id(&thread) else {
            break;
        };
        if parent == current || chain.iter().any(|(child, _)| child == &parent) {
            break;
        }
        chain.push((current.clone(), parent.clone()));
        current = parent;
    }
    mark_rejected(cutex_session_id, root_thread_id, thread_id);
    Ok(false)
}

/// Learns spawn edges from a notification emitted on a trusted thread:
/// `collabAgentToolCall` with `tool: spawnAgent` (receivers are the newly
/// spawned agents), `subAgentActivity` with `kind: started`, and
/// `thread/started` carrying a sub-agent `Thread`.
pub fn learn_from_notification(cutex_session_id: &str, root_thread_id: &str, raw: &Value) {
    let Some(method) = raw.get("method").and_then(Value::as_str) else {
        return;
    };
    let Some(params) = raw.get("params") else {
        return;
    };
    let edges: Vec<(String, String)> = match method {
        "item/started" | "item/completed" => {
            let (Some(sender), Some(item)) =
                (params.get("threadId").and_then(Value::as_str), params.get("item"))
            else {
                return;
            };
            match item.get("type").and_then(Value::as_str) {
                Some("collabAgentToolCall")
                    if item.get("tool").and_then(Value::as_str) == Some("spawnAgent")
                        && item.get("senderThreadId").and_then(Value::as_str) == Some(sender) =>
                {
                    item.get("receiverThreadIds")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(|child| (child.to_string(), sender.to_string()))
                        .collect()
                }
                Some("subAgentActivity")
                    if item.get("kind").and_then(Value::as_str) == Some("started") =>
                {
                    item.get("agentThreadId")
                        .and_then(Value::as_str)
                        .map(|child| vec![(child.to_string(), sender.to_string())])
                        .unwrap_or_default()
                }
                _ => return,
            }
        }
        "thread/started" => {
            let Some(thread) = params.get("thread") else {
                return;
            };
            match (thread.get("id").and_then(Value::as_str), parent_thread_id(thread)) {
                (Some(child), Some(parent)) => vec![(child.to_string(), parent)],
                _ => return,
            }
        }
        _ => return,
    };
    if edges.is_empty() {
        return;
    }
    with_session(cutex_session_id, root_thread_id, |lineage| {
        for (child, parent) in edges {
            lineage.record_edge(&child, &parent);
        }
    });
}

/// How an event addressed to a non-root thread should be handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForeignThreadRoute {
    /// Verified descendant: publish now.
    Descendant,
    /// Unknown lineage: buffered. `resolve` asks the caller to start a
    /// lineage lookup for the thread.
    Buffered { resolve: bool },
    /// Ancestry does not reach the root: ignore.
    Rejected,
}

/// Classifies (and if unknown, buffers) an event for a non-root thread.
pub fn route_foreign_event(
    cutex_session_id: &str,
    root_thread_id: &str,
    thread_id: &str,
    event: &AppServerEvent,
) -> ForeignThreadRoute {
    let now = Instant::now();
    with_session(cutex_session_id, root_thread_id, |lineage| {
        if lineage.is_descendant(thread_id) {
            return ForeignThreadRoute::Descendant;
        }
        if lineage.rejected.contains(thread_id) {
            return ForeignThreadRoute::Rejected;
        }
        let pending = lineage.pending.entry(thread_id.to_string()).or_default();
        pending.first_seen.get_or_insert(now);
        let resolve = pending
            .resolve_started
            .is_none_or(|started| now.duration_since(started) >= RESOLVE_RETRY);
        if resolve {
            pending.resolve_started = Some(now);
        }
        let sequence = lineage.next_sequence;
        lineage.next_sequence += 1;
        lineage
            .buffered
            .insert(sequence, (thread_id.to_string(), event.clone()));
        while lineage.buffered.len() > MAX_BUFFERED_EVENTS {
            lineage.buffered.pop_first();
        }
        ForeignThreadRoute::Buffered { resolve }
    })
}

/// Marks a thread as not descending from the root and drops its buffer.
pub fn mark_rejected(cutex_session_id: &str, root_thread_id: &str, thread_id: &str) {
    with_session(cutex_session_id, root_thread_id, |lineage| {
        if lineage.is_descendant(thread_id) {
            return;
        }
        lineage.rejected.insert(thread_id.to_string());
        lineage.pending.remove(thread_id);
        lineage.buffered.retain(|_, (thread, _)| thread != thread_id);
    });
}

/// Buffered events whose thread is now a verified descendant, in arrival
/// order. Buffers of rejected threads and entries older than the TTL are
/// discarded; the number of discarded events is returned alongside.
pub fn take_ready_events(
    cutex_session_id: &str,
    root_thread_id: &str,
) -> (Vec<AppServerEvent>, usize) {
    let now = Instant::now();
    with_session(cutex_session_id, root_thread_id, |lineage| {
        if lineage.buffered.is_empty() && lineage.pending.is_empty() {
            return (Vec::new(), 0);
        }
        let mut ready_threads = HashSet::new();
        let mut dropped_threads = HashSet::new();
        for (thread_id, pending) in &lineage.pending {
            if lineage.is_descendant(thread_id) {
                ready_threads.insert(thread_id.clone());
            } else if lineage.rejected.contains(thread_id)
                || pending
                    .first_seen
                    .is_some_and(|first| now.duration_since(first) >= BUFFER_TTL)
            {
                dropped_threads.insert(thread_id.clone());
            }
        }
        if ready_threads.is_empty() && dropped_threads.is_empty() {
            return (Vec::new(), 0);
        }
        let mut ready = Vec::new();
        let mut dropped = 0;
        lineage.buffered.retain(|_, (thread, event)| {
            if ready_threads.contains(thread) {
                ready.push(event.clone());
                false
            } else if dropped_threads.contains(thread) {
                dropped += 1;
                false
            } else {
                true
            }
        });
        for thread in ready_threads.iter().chain(dropped_threads.iter()) {
            lineage.pending.remove(thread);
        }
        (ready, dropped)
    })
}

/// Drops buffered (unresolved) events, e.g. after a runtime disconnect.
/// Verified edges are kept: lineage is immutable for a thread id.
pub fn clear_pending(cutex_session_id: &str, root_thread_id: &str) {
    with_session(cutex_session_id, root_thread_id, |lineage| {
        lineage.pending.clear();
        lineage.buffered.clear();
    });
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::app_server::protocol::RpcNotification;

    fn session() -> String {
        format!("lineage-test-{}", uuid::Uuid::new_v4())
    }

    fn notification(thread_id: &str) -> AppServerEvent {
        let raw = json!({ "method": "turn/started", "params": { "threadId": thread_id } });
        AppServerEvent::Notification(RpcNotification {
            method: "turn/started".to_string(),
            params: raw.get("params").cloned(),
            raw,
        })
    }

    fn thread(id: &str, parent: Option<&str>) -> Value {
        match parent {
            Some(parent) => json!({
                "id": id,
                "parentThreadId": parent,
                "source": { "subAgent": { "thread_spawn": { "parent_thread_id": parent, "depth": 1 } } }
            }),
            None => json!({ "id": id, "parentThreadId": null, "source": "vscode" }),
        }
    }

    #[test]
    fn verify_walks_transitive_parents_and_caches() {
        let session = session();
        let mut reads = 0;
        let reader = |id: &str| -> Result<Option<Value>, LineageReadError> {
            Ok(match id {
                "grandchild" => Some(thread("grandchild", Some("child"))),
                "child" => Some(thread("child", Some("root"))),
                _ => None,
            })
        };
        let verified = verify_descendant(&session, "root", "grandchild", |id| {
            reads += 1;
            reader(id)
        })
        .expect("verify");
        assert!(verified);
        assert_eq!(reads, 2);
        assert!(is_descendant(&session, "root", "child"));
        assert!(is_descendant(&session, "root", "grandchild"));
        // Cached: no further reads.
        assert!(verify_descendant(&session, "root", "grandchild", |_| panic!("cached")).unwrap());
    }

    #[test]
    fn verify_rejects_unrelated_root_and_unknown_threads() {
        let session = session();
        let reader = |id: &str| -> Result<Option<Value>, LineageReadError> {
            Ok(match id {
                "other-child" => Some(thread("other-child", Some("other-root"))),
                "other-root" => Some(thread("other-root", None)),
                "cycle-a" => Some(thread("cycle-a", Some("cycle-b"))),
                "cycle-b" => Some(thread("cycle-b", Some("cycle-a"))),
                "liar" => Some(thread("someone-else", Some("root"))),
                _ => None,
            })
        };
        for id in ["other-child", "other-root", "missing", "cycle-a", "liar"] {
            assert!(!verify_descendant(&session, "root", id, reader).unwrap(), "{id}");
        }
        assert!(!verify_descendant(&session, "root", "root", reader).unwrap());
        assert!(!is_descendant(&session, "root", "other-child"));
        assert_eq!(
            route_foreign_event(&session, "root", "other-child", &notification("other-child")),
            ForeignThreadRoute::Rejected
        );
    }

    #[test]
    fn unavailable_reader_is_not_a_rejection() {
        let session = session();
        let result = verify_descendant(&session, "root", "child", |_| {
            Err(LineageReadError::Unavailable("offline".to_string()))
        });
        assert!(result.is_err());
        assert!(matches!(
            route_foreign_event(&session, "root", "child", &notification("child")),
            ForeignThreadRoute::Buffered { .. }
        ));
    }

    #[test]
    fn spawn_items_on_trusted_threads_teach_edges() {
        let session = session();
        learn_from_notification(
            &session,
            "root",
            &json!({ "method": "item/completed", "params": { "threadId": "root", "item": {
                "type": "collabAgentToolCall", "tool": "spawnAgent",
                "senderThreadId": "root", "receiverThreadIds": ["child"]
            } } }),
        );
        assert!(is_descendant(&session, "root", "child"));
        learn_from_notification(
            &session,
            "root",
            &json!({ "method": "item/started", "params": { "threadId": "child", "item": {
                "type": "subAgentActivity", "kind": "started",
                "agentThreadId": "grandchild", "agentPath": "/root/a/b"
            } } }),
        );
        assert!(is_descendant(&session, "root", "grandchild"));
        // Messaging tools and untrusted senders never create edges.
        learn_from_notification(
            &session,
            "root",
            &json!({ "method": "item/completed", "params": { "threadId": "child", "item": {
                "type": "collabAgentToolCall", "tool": "sendInput",
                "senderThreadId": "child", "receiverThreadIds": ["stranger-1"]
            } } }),
        );
        learn_from_notification(
            &session,
            "root",
            &json!({ "method": "item/completed", "params": { "threadId": "stranger", "item": {
                "type": "collabAgentToolCall", "tool": "spawnAgent",
                "senderThreadId": "stranger", "receiverThreadIds": ["stranger-2"]
            } } }),
        );
        assert!(!is_descendant(&session, "root", "stranger-1"));
        assert!(!is_descendant(&session, "root", "stranger-2"));
        // A child can never become an ancestor of the root.
        assert!(!record_edge(&session, "root", "root", "child"));
    }

    #[test]
    fn buffered_events_flush_in_order_once_verified() {
        let session = session();
        assert_eq!(
            route_foreign_event(&session, "root", "child", &notification("child")),
            ForeignThreadRoute::Buffered { resolve: true }
        );
        assert_eq!(
            route_foreign_event(&session, "root", "stranger", &notification("stranger")),
            ForeignThreadRoute::Buffered { resolve: true }
        );
        assert_eq!(
            route_foreign_event(&session, "root", "child", &notification("child")),
            ForeignThreadRoute::Buffered { resolve: false }
        );
        assert_eq!(take_ready_events(&session, "root"), (Vec::new(), 0));
        assert!(record_edge(&session, "root", "child", "root"));
        mark_rejected(&session, "root", "stranger");
        let (ready, dropped) = take_ready_events(&session, "root");
        assert_eq!(ready, vec![notification("child"), notification("child")]);
        assert_eq!(dropped, 0, "rejected buffers are removed eagerly");
        assert_eq!(
            route_foreign_event(&session, "root", "child", &notification("child")),
            ForeignThreadRoute::Descendant
        );
    }

    #[test]
    fn rebinding_the_root_resets_lineage() {
        let session = session();
        assert!(record_edge(&session, "root", "child", "root"));
        assert!(!is_descendant(&session, "new-root", "child"));
        assert!(!is_descendant(&session, "root", "child"));
    }
}
