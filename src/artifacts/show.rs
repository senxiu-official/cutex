//! `artifact_publish {show: true}` (cutex-desktop
//! `docs/research/agent-side-display-20261013.md` §3.1): after a successful
//! publish, ask the desktop to open the artifact beside the publisher's
//! conversation with the host event `cutex/artifact/showRequested`.
//!
//! Rate limited per publishing session (token bucket: burst
//! [`SHOW_BURST`], then one per [`SHOW_REFILL`]). A limited publish still
//! succeeds; it only emits nothing. Nothing is persisted (capability
//! `artifact-show`; older desktops ignore the event).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{json, Value};

use super::store::Outcome;

/// Owner capability name (`GET /v2/capabilities`).
pub const CAPABILITY: &str = "artifact-show";
pub const EVENT_SHOW_REQUESTED: &str = "cutex/artifact/showRequested";
pub const SHOW_BURST: f64 = 3.0;
pub const SHOW_REFILL: Duration = Duration::from_secs(5);
/// Most sessions tracked at once (full buckets are dropped first).
const MAX_SESSIONS: usize = 1024;

#[derive(Default)]
pub struct ShowLimiter {
    buckets: HashMap<String, (f64, Instant)>,
}

impl ShowLimiter {
    /// Take one show for `session` at `now`.
    pub fn allow(&mut self, session: &str, now: Instant) -> bool {
        let refill = |(tokens, at): (f64, Instant)| {
            (tokens + now.saturating_duration_since(at).as_secs_f64() / SHOW_REFILL.as_secs_f64()).min(SHOW_BURST)
        };
        if self.buckets.len() >= MAX_SESSIONS && !self.buckets.contains_key(session) {
            // A full bucket is the same as no entry.
            self.buckets.retain(|_, bucket| refill(*bucket) < SHOW_BURST);
            if self.buckets.len() >= MAX_SESSIONS {
                return false;
            }
        }
        let tokens = self.buckets.get(session).copied().map(refill).unwrap_or(SHOW_BURST);
        if tokens < 1.0 {
            self.buckets.insert(session.to_string(), (tokens, now));
            return false;
        }
        self.buckets.insert(session.to_string(), (tokens - 1.0, now));
        true
    }
}

/// The process-wide limiter (the Agent Bus process serves the MCP tool).
pub fn allow(session: &str) -> bool {
    static LIMITER: OnceLock<Mutex<ShowLimiter>> = OnceLock::new();
    LIMITER
        .get_or_init(Default::default)
        .lock()
        .map(|mut limiter| limiter.allow(session, Instant::now()))
        .unwrap_or(false)
}

/// `cutex/artifact/showRequested` params.
pub fn show_event(outcome: &Outcome, session: &str, display_name: Option<&str>, now: DateTime<Utc>) -> Value {
    json!({
        "artifactId": outcome.artifact.artifact_id,
        "version": outcome.version.version,
        "ref": outcome.version.reference,
        "scope": outcome.artifact.scope,
        "name": outcome.artifact.name,
        "kind": outcome.artifact.kind,
        "mediaType": outcome.version.media_type,
        "cutexSessionId": session,
        "displayName": display_name,
        "hostId": crate::platform::host::current_host_name(),
        "newVersion": outcome.new_version,
        "requestedAt": now.to_rfc3339_opts(SecondsFormat::Millis, true),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_then_one_per_refill_per_session() {
        let mut limiter = ShowLimiter::default();
        let start = Instant::now();
        for _ in 0..3 {
            assert!(limiter.allow("cutex.a", start));
        }
        assert!(!limiter.allow("cutex.a", start), "burst used up");
        assert!(limiter.allow("cutex.b", start), "other sessions have their own bucket");
        assert!(!limiter.allow("cutex.a", start + Duration::from_secs(4)));
        // A refused call does not reset the refill: 4 s + 1 s = one token.
        assert!(limiter.allow("cutex.a", start + Duration::from_secs(5)));
        assert!(!limiter.allow("cutex.a", start + Duration::from_secs(6)));
        // A long pause refills to the burst, never beyond it.
        let later = start + Duration::from_secs(600);
        for _ in 0..3 {
            assert!(limiter.allow("cutex.a", later));
        }
        assert!(!limiter.allow("cutex.a", later));
    }

    #[test]
    fn the_session_table_is_bounded() {
        let mut limiter = ShowLimiter::default();
        let start = Instant::now();
        for n in 0..MAX_SESSIONS {
            for _ in 0..3 {
                assert!(limiter.allow(&format!("cutex.{n}"), start));
            }
        }
        assert!(!limiter.allow("cutex.new", start), "full of busy sessions: refuse, do not grow");
        assert!(limiter.allow("cutex.new", start + Duration::from_secs(60)), "refilled buckets are dropped");
        assert!(limiter.buckets.len() <= MAX_SESSIONS);
    }
}
