//! Asks: subagent-style call/return between persistent cutex agents
//! (cutex-desktop `docs/cutex-agent-asks.md`, phase P1). User-facing texts
//! call an ask 「委托」.
//!
//! - [`model`]: the durable records (ask, attempt, answer revisions,
//!   question, delivery, admission lease, event outbox).
//! - [`store`]: one file-locked store shared by the Agent Bus process (tool
//!   calls) and the management process (scheduler, runtime events), plus the
//!   daemon-owned artifact store.
//! - [`caps`]: capabilities and the `caps(target) ⊄ caps(requester)` gate
//!   condition.
//!
//! Process split: agent tools run in the Agent Bus process and only write
//! records; the management process, which holds the runtime connections,
//! admits asks (scheduler), observes their turns, captures answers, sends
//! question responses and interrupts. Management v2 events are queued as
//! store notices and published by the management process, so clients get
//! them live whichever process made the change.

pub mod answer;
pub mod archive;
pub mod agent_pause;
pub mod result_coalescing;
pub mod awaits;
pub mod budget;
pub mod caps;
pub mod children;
pub mod commands;
pub mod control;
pub mod continuation;
pub mod routing_policy;
pub mod receipt_uncertainty;
pub mod operations;
pub mod project_control;
pub mod pause_admission;
pub mod lineage;
pub mod create_task;
pub mod delivery;
pub mod directory;
pub mod gate;
pub mod lifecycle;
pub mod model;
pub mod observe;
pub mod questions;
pub mod render;
pub mod returns;
pub mod revisions;
pub mod scheduler;
pub mod store;
pub mod short_ids;
pub mod tools;
pub mod view;
pub mod views;
pub mod waits;
pub mod wake;

#[cfg(test)]
mod control_load_tests;

/// Inline brief cap (§5.4). Longer briefs go to an artifact.
pub const INLINE_BRIEF_MAX_BYTES: usize = 4 * 1024;
/// Inline answer cap (§5.4). Longer answers are truncated with a marker
/// and paged with `ask_read`.
pub const INLINE_ANSWER_MAX_BYTES: usize = 8 * 1024;
/// Hard upper bound of the raw text of any ASK/ANSWER/QUESTION event: the
/// ExternalInput transport cap. What a receiver accepts is usually far less
/// (canonical byte policy, default 10,000 serialized bytes; no cutex launch
/// raises it): texts are rendered against the receiver's budget
/// ([`crate::app_server::external_input::size`]) and long parts become
/// `delegation_read` pointers (render levels).
pub const EVENT_MAX_BYTES: usize = crate::app_server::external_input::size::TRANSPORT_TEXT_MAX;
/// Render levels of an ASK / continuation: 0 = everything inline, each
/// further level replaces more inline text with pointers.
pub const RENDER_LEVELS: u8 = 4;
/// Retryable native submit failures (transport or unknown RPC errors of a
/// submit, failed status queries while its outcome is unknown) back off
/// exponentially from [`SUBMIT_RETRY_BASE_SECS`] up to
/// [`SUBMIT_RETRY_MAX_SECS`]; after [`SUBMIT_RETRY_LIMIT`] failures in a row
/// the ask fails (`submit_failed`), or ends `interrupted(outcomeUnknown)`
/// while the runtime may hold it. A rejection by the receiver byte policy is
/// never retried (`event_too_large`).
pub const SUBMIT_RETRY_BASE_SECS: i64 = 5;
pub const SUBMIT_RETRY_MAX_SECS: i64 = 600;
pub const SUBMIT_RETRY_LIMIT: u32 = 8;
/// How often one delegation may go back to the queue after its attempt
/// failed on the target's exhausted Claude usage limit (it is then held
/// `blocked(quota)` until the limit resets and redelivered as a new
/// attempt). Past this the attempt fails the delegation as any failure does.
pub const QUOTA_REQUEUE_LIMIT: u32 = 5;
/// Failure codes of asks the runtime never ran.
pub const EVENT_TOO_LARGE: &str = "event_too_large";
pub const SUBMIT_FAILED: &str = "submit_failed";
/// Results whose event submit outcome stayed unknown past
/// [`EVENT_UNAVAILABLE_GRACE_SECS`]: made passive (never re-sent).
pub const RESULTS_OUTCOME_UNKNOWN: &str = "results_outcome_unknown";
/// How long an event lease may stay unreconcilable (status query failing,
/// or a terminal receipt without a turn) before its results are made
/// passive and the requester's admission is released.
pub const EVENT_UNAVAILABLE_GRACE_SECS: i64 = 600;
/// Queued asks per target (`target_overloaded`).
pub const MAX_QUEUED_PER_TARGET: usize = 50;
/// Hard cap of an ask brief (inline or artifact).
pub const MAX_BRIEF_BYTES: usize = 256 * 1024;
/// Default and maximum `agent_wait` timeouts (§5.8).
pub const WAIT_DEFAULT_MS: u64 = 30_000;
pub const WAIT_MAX_MS: u64 = 120_000;
/// Questions fall through to the owner after this long (§5.9).
pub const QUESTION_TIMEOUT_SECS: i64 = 10 * 60;
/// `blocked(...)` notifies the requester after this long (§5.2).
pub const BLOCKED_TIMEOUT_SECS: i64 = 10 * 60;

/// A short, unique, prefixed server id.
pub fn new_id(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
}

/// The first line of a brief, bounded, for titles.
pub fn title_of(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("");
    truncate_chars(line, 120)
}

/// At most `max` characters, with an ellipsis when cut.
pub fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The longest prefix of `text` of at most `max` bytes on a char boundary.
pub fn byte_prefix(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

pub fn now_rfc3339(now: chrono::DateTime<chrono::Utc>) -> String {
    now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn parse_time(value: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.with_timezone(&chrono::Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_helpers_respect_boundaries() {
        assert_eq!(title_of("\n  Review the parser  \nmore"), "Review the parser");
        assert_eq!(byte_prefix("aé", 2), "a");
        assert_eq!(truncate_chars("abcdef", 4), "abc…");
        assert!(new_id("ask").starts_with("ask-"));
    }
}
