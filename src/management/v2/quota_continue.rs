//! Claude quota auto-continue (capability `claude-quota-auto-continue`,
//! owner request 2026-10-13: "claude撞墙之后能不能搞一个自动回复").
//!
//! When a Claude agent's turn fails because its subscription / account
//! usage limit is exhausted (adapter turn error `codexErrorInfo`
//! `usageLimitExceeded`, or `rateLimitExceeded` with a known reset), Cutex
//! records when that limit resets and, at the reset plus a small per-agent
//! jitter (30–119 s), sends the agent one continuation as owner input
//! ([`CONTINUE_TEXT`], origin `automation` / [`CLIENT_ID`], strategy `auto`,
//! never starting an offline runtime).
//!
//! - Per agent, default on; `PATCH /v2/quota-auto-continue/{sessionId}
//!   {enabled}` stores the switch (only `enabled: false` is kept).
//! - At most one continuation per reset window: a window (its `resetAt`)
//!   that already fired is never scheduled again, so a continuation that
//!   hits the limit again waits for the owner (or a later reset).
//! - A pending continuation is cancelled by the owner's own message,
//!   `cutex/turn/stop`, taking the runtime offline / closing it, retiring /
//!   deleting the agent, `DELETE /v2/quota-auto-continue/{sessionId}/pending`,
//!   turning the switch off, or a later turn of the agent that completed
//!   (it is not stuck any more). At fire time it is skipped (recorded in
//!   `last`) when the agent is archived, offline, on another runtime
//!   generation, or its queue / admission is paused.
//! - Reset time: the adapter's `account/rateLimits/read` rows that are
//!   `rejected` or at 100 % (latest reset of them), else the Cutex Claude
//!   account quota reading (`profile_quota_claude`) windows at 100 %. With
//!   no known reset (or a window that already got its continuation and is
//!   still reported exhausted) an *estimated* probe is scheduled instead
//!   (`pending.estimated`): 30 min after the failure, doubling per further
//!   estimated probe up to 4 h, at most 8 in a row (then nothing is
//!   scheduled: `last.result = "skipped"`, `reason = "resetUnknown"`); a
//!   completed turn resets the count.
//! - Failure detection: the adapter's `codexErrorInfo` (`usageLimitExceeded`
//!   / `rateLimitExceeded`, claude-r9 and later) or, for older adapters
//!   (claude-r5 / r7 report `other`), the adapter-authored error text
//!   `Official Claude returned an error (rate_limit|billing_error)` /
//!   `… rejected the request (rate_limit|billing_error)`.
//! - Quota hold (owner report 2026-10-14: a Claude Director's delegation
//!   failed on `rate_limit` and nothing ran it again after the reset): while
//!   a continuation is pending the agent counts as quota-blocked for the
//!   asks scheduler (`blocked(quota)`), and an ask attempt that failed on the
//!   limit goes back to the queue instead of failing the delegation
//!   (`asks::observe::observe_with`). At the reset the hold ends and the
//!   scheduler redelivers it; the continuation text is then not sent
//!   (`last.reason = "asksRedelivered"`): the redelivered delegation is the
//!   work. The hold is taken synchronously at the failed `turn/completed`
//!   (estimated until the reset is read), before the asks observer runs.
//!
//! Routes (root, incl. client / device tokens):
//! - `GET /v2/quota-auto-continue[?sessionId=]` → `{hostId, sessions:
//!   [QuotaAutoContinue]}` (with `sessionId`: exactly that agent, defaults
//!   filled in; without: every agent with a stored switch, pending
//!   continuation or last outcome).
//! - `PATCH /v2/quota-auto-continue/{sessionId}` `{enabled: bool}` →
//!   `{quotaAutoContinue}`.
//! - `DELETE /v2/quota-auto-continue/{sessionId}/pending` → `{quotaAutoContinue}`
//!   (the owner cancels the pending continuation; idempotent).
//!
//! `QuotaAutoContinue` = `{cutexSessionId, enabled, pending: {at, resetAt,
//! accountId, turnId, scheduledAt} | null, last: {at, result, reason,
//! resetAt} | null}` (times RFC 3339). Host event
//! `cutex/quotaAutoContinue/changed` `{cutexSessionId, reason,
//! quotaAutoContinue, hostId}`, reason `scheduled` | `cancelled` | `fired` |
//! `skipped` | `failed` | `enabled` | `disabled`.
//!
//! State: `<runtime>/quota-continue/state-v1.json` (owner-private), written
//! only by the Management process.

use std::collections::BTreeMap;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Mutex;

use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::http::server::{write_json_response, SimpleHttpRequest};

pub const FEATURE: &str = "claude-quota-auto-continue";
pub const PATH: &str = "/v2/quota-auto-continue";
pub const CHANGED_EVENT: &str = "cutex/quotaAutoContinue/changed";
/// `origin.clientId` of the continuation input.
pub const CLIENT_ID: &str = "cutex-quota-continue";
/// The continuation the agent receives.
pub const CONTINUE_TEXT: &str =
    "继续 / Continue. (Sent automatically by Cutex: the Claude usage limit that stopped your last turn has reset. Continue the interrupted work where you left off.)";
const STATE_VERSION: u32 = 1;
const MIN_JITTER_SECS: i64 = 30;
const JITTER_SPAN_SECS: u64 = 90;
/// First estimated probe after a failure with no known reset.
const ESTIMATE_BASE_SECS: i64 = 30 * 60;
/// Longest wait of one estimated probe.
const ESTIMATE_MAX_SECS: i64 = 4 * 3600;
/// Estimated probes in a row before giving up (about a day in all).
pub const MAX_ESTIMATED_PROBES: u32 = 8;

pub const EXPORTED_SCHEMA: super::schema_export::ExportedSchema = super::schema_export::ExportedSchema {
    name: "quota-auto-continue",
    title: "Claude quota auto-continue",
    routes: &[
        "GET /v2/quota-auto-continue",
        "PATCH /v2/quota-auto-continue/{sessionId}",
        "DELETE /v2/quota-auto-continue/{sessionId}/pending",
        "host event cutex/quotaAutoContinue/changed",
    ],
    text: include_str!("schema/export/quota-auto-continue.json"),
};

// ---------------------------------------------------------------------------
// State

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StateFile {
    pub version: u32,
    pub sessions: BTreeMap<String, Entry>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Entry {
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<Pending>,
    /// Unix seconds of the reset window that already got its continuation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fired_reset_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last: Option<Outcome>,
    /// Estimated probes (no known reset) fired since the last completed turn.
    #[serde(skip_serializing_if = "is_zero")]
    pub unknown_probes: u32,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pending {
    /// When the continuation goes out (reset + jitter), RFC 3339.
    pub at: String,
    /// Unix seconds the exhausted limit resets.
    pub reset_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub runtime_generation: u64,
    pub scheduled_at: String,
    /// The reset is not known: `reset_at` is an estimate (a probe).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub estimated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub at: String,
    /// `fired` | `skipped` | `cancelled` | `failed`.
    pub result: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<i64>,
}

fn rfc3339(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Wait before the estimated probe number `probes` (0-based).
pub fn estimate_delay_secs(probes: u32) -> i64 {
    ESTIMATE_BASE_SECS.saturating_mul(1_i64 << probes.min(16)).min(ESTIMATE_MAX_SECS)
}

/// Whether an adapter-authored turn error text says the Claude usage /
/// rate limit was hit (adapters before claude-r9 report `codexErrorInfo`
/// `other` for these). Only the adapter's own fixed wording is matched,
/// never provider text.
pub fn quota_error_text(message: &str) -> bool {
    ["rate_limit", "billing_error"].iter().any(|code| {
        message.starts_with(&format!("Official Claude returned an error ({code})."))
            || (message.starts_with("Claude API account '") && message.contains(&format!(" rejected the request ({code}).")))
    })
}

fn unix_rfc3339(secs: i64) -> Option<String> {
    Utc.timestamp_opt(secs, 0).single().map(rfc3339)
}

/// Per-agent jitter after the reset (stable, 30..=119 s): agents on one
/// account don't all start at the same second.
pub fn jitter_secs(session: &str) -> i64 {
    // FNV-1a: stable across processes and builds.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in session.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    MIN_JITTER_SECS + (hash % JITTER_SPAN_SECS) as i64
}

impl StateFile {
    fn entry(&mut self, session: &str) -> &mut Entry {
        self.sessions.entry(session.to_string()).or_default()
    }

    fn prune(&mut self, session: &str) {
        if self.sessions.get(session).is_some_and(|entry| *entry == Entry::default()) {
            self.sessions.remove(session);
        }
    }

    pub fn enabled(&self, session: &str) -> bool {
        !self.sessions.get(session).is_some_and(|entry| entry.disabled)
    }

    /// Schedule the continuation for a turn that failed on an exhausted
    /// limit resetting at `reset_at`. Returns whether anything changed.
    #[allow(clippy::too_many_arguments)]
    pub fn schedule(
        &mut self,
        session: &str,
        reset_at: i64,
        account_id: Option<String>,
        turn_id: Option<String>,
        generation: u64,
        now: DateTime<Utc>,
    ) -> bool {
        if !self.enabled(session) {
            return false;
        }
        let entry = self.entry(session);
        if entry.fired_reset_at == Some(reset_at) {
            // This window already got its one continuation and is still
            // reported exhausted (a stale reading): probe instead of
            // waiting for the owner.
            entry.last = Some(Outcome {
                at: rfc3339(now),
                result: "skipped".into(),
                reason: Some("windowAlreadyContinued".into()),
                reset_at: Some(reset_at),
            });
            self.schedule_estimated(session, account_id, turn_id, generation, now);
            return true;
        }
        if entry.pending.as_ref().is_some_and(|pending| {
            pending.reset_at == reset_at && pending.runtime_generation == generation && !pending.estimated
        }) {
            return false;
        }
        let Some(at) = Utc.timestamp_opt(reset_at.max(now.timestamp()) + jitter_secs(session), 0).single() else {
            return false;
        };
        entry.unknown_probes = 0;
        entry.pending = Some(Pending {
            at: rfc3339(at),
            reset_at,
            account_id,
            turn_id,
            runtime_generation: generation,
            scheduled_at: rfc3339(now),
            estimated: false,
        });
        true
    }

    fn estimated_pending(
        session: &str,
        probes: u32,
        account_id: Option<String>,
        turn_id: Option<String>,
        generation: u64,
        now: DateTime<Utc>,
    ) -> Option<Pending> {
        let reset_at = now.timestamp() + estimate_delay_secs(probes);
        let at = Utc.timestamp_opt(reset_at + jitter_secs(session), 0).single()?;
        Some(Pending {
            at: rfc3339(at),
            reset_at,
            account_id,
            turn_id,
            runtime_generation: generation,
            scheduled_at: rfc3339(now),
            estimated: true,
        })
    }

    /// Hold the agent right at a quota failure, before its reset is read:
    /// an estimated pending continuation unless one is pending already (or
    /// the estimated probes are used up). The reset lookup then replaces it
    /// ([`Self::schedule`] / [`Self::schedule_estimated`]).
    pub fn hold(&mut self, session: &str, turn_id: Option<String>, generation: u64, now: DateTime<Utc>) -> bool {
        if !self.enabled(session) {
            return false;
        }
        let probes = match self.sessions.get(session) {
            Some(entry) if entry.pending.is_some() => return false,
            Some(entry) => entry.unknown_probes,
            None => 0,
        };
        if probes >= MAX_ESTIMATED_PROBES {
            return false;
        }
        let Some(pending) = Self::estimated_pending(session, probes, None, turn_id, generation, now) else {
            return false;
        };
        self.entry(session).pending = Some(pending);
        true
    }

    /// A quota failure with no known reset: schedule the next estimated
    /// probe, or give up after [`MAX_ESTIMATED_PROBES`] fired in a row.
    pub fn schedule_estimated(
        &mut self,
        session: &str,
        account_id: Option<String>,
        turn_id: Option<String>,
        generation: u64,
        now: DateTime<Utc>,
    ) -> bool {
        if !self.enabled(session) {
            return false;
        }
        let entry = self.entry(session);
        let probes = entry.unknown_probes;
        if probes >= MAX_ESTIMATED_PROBES {
            if entry.pending.as_ref().is_some_and(|pending| pending.estimated) {
                entry.pending = None;
            }
            entry.last = Some(Outcome {
                at: rfc3339(now),
                result: "skipped".into(),
                reason: Some("resetUnknown".into()),
                reset_at: None,
            });
            return true;
        }
        // The hold taken at this failure already is this probe (a repeated
        // `turn/completed` lands here too: probes are counted when one fires).
        let held = entry.pending.as_ref().is_some_and(|pending| {
            pending.estimated && pending.runtime_generation == generation && pending.turn_id == turn_id
        });
        if held {
            if let Some(pending) = entry.pending.as_mut() {
                pending.account_id = account_id;
            }
        } else if let Some(pending) = Self::estimated_pending(session, probes, account_id, turn_id, generation, now) {
            entry.pending = Some(pending);
        }
        true
    }

    /// A turn of the agent completed: it is not stuck any more. Cancels a
    /// pending continuation and resets the estimated probes.
    pub fn turn_completed(&mut self, session: &str, now: DateTime<Utc>) -> bool {
        let cancelled = self.cancel(session, "turnCompleted", now);
        let reset = match self.sessions.get_mut(session) {
            Some(entry) if entry.unknown_probes > 0 => {
                entry.unknown_probes = 0;
                true
            }
            _ => false,
        };
        self.prune(session);
        cancelled || reset
    }

    /// The agent waits for its usage limit to reset (a continuation is
    /// pending): the asks scheduler holds its delegations `blocked(quota)`.
    pub fn quota_held(&self, session: &str) -> bool {
        self.sessions.get(session).is_some_and(|entry| !entry.disabled && entry.pending.is_some())
    }

    /// Drop the pending continuation. Returns whether one existed.
    pub fn cancel(&mut self, session: &str, reason: &str, now: DateTime<Utc>) -> bool {
        let Some(entry) = self.sessions.get_mut(session) else {
            return false;
        };
        let Some(pending) = entry.pending.take() else {
            return false;
        };
        entry.last = Some(Outcome {
            at: rfc3339(now),
            result: "cancelled".into(),
            reason: Some(reason.into()),
            reset_at: Some(pending.reset_at),
        });
        true
    }

    /// Store the per-agent switch; turning it off cancels a pending
    /// continuation. Returns whether anything changed.
    pub fn set_enabled(&mut self, session: &str, enabled: bool, now: DateTime<Utc>) -> bool {
        if self.enabled(session) == enabled {
            return false;
        }
        if !enabled {
            self.cancel(session, "disabled", now);
        }
        self.entry(session).disabled = !enabled;
        self.prune(session);
        true
    }

    /// Sessions whose continuation is due at `now`.
    pub fn due(&self, now: DateTime<Utc>) -> Vec<String> {
        self.sessions
            .iter()
            .filter(|(_, entry)| {
                entry.pending.as_ref().and_then(|pending| crate::asks::parse_time(&pending.at)).is_some_and(|at| at <= now)
            })
            .map(|(session, _)| session.clone())
            .collect()
    }

    /// The earliest pending `at`.
    pub fn next_at(&self) -> Option<DateTime<Utc>> {
        self.sessions
            .values()
            .filter_map(|entry| entry.pending.as_ref().and_then(|pending| crate::asks::parse_time(&pending.at)))
            .min()
    }

    /// Take the due continuation of `session` and mark its window used (at
    /// most once, whatever the send then does).
    pub fn take_due(&mut self, session: &str, now: DateTime<Utc>) -> Option<Pending> {
        let entry = self.sessions.get_mut(session)?;
        let due = entry.pending.as_ref().and_then(|pending| crate::asks::parse_time(&pending.at)).is_some_and(|at| at <= now);
        if !due || entry.disabled {
            return None;
        }
        let pending = entry.pending.take()?;
        entry.fired_reset_at = Some(pending.reset_at);
        if pending.estimated {
            entry.unknown_probes += 1;
        }
        Some(pending)
    }

    pub fn record(&mut self, session: &str, result: &str, reason: Option<&str>, reset_at: Option<i64>, now: DateTime<Utc>) {
        self.entry(session).last = Some(Outcome {
            at: rfc3339(now),
            result: result.into(),
            reason: reason.map(str::to_string),
            reset_at,
        });
    }

    /// The client view of one agent (defaults when nothing is stored).
    pub fn view(&self, session: &str) -> Value {
        let entry = self.sessions.get(session).cloned().unwrap_or_default();
        json!({
            "cutexSessionId": session,
            "enabled": !entry.disabled,
            "pending": entry.pending.as_ref().map(|pending| json!({
                "at": pending.at,
                "resetAt": unix_rfc3339(pending.reset_at),
                "accountId": pending.account_id,
                "turnId": pending.turn_id,
                "scheduledAt": pending.scheduled_at,
                "estimated": pending.estimated,
            })),
            "last": entry.last.as_ref().map(|last| json!({
                "at": last.at,
                "result": last.result,
                "reason": last.reason,
                "resetAt": last.reset_at.and_then(unix_rfc3339),
            })),
        })
    }
}

// ---------------------------------------------------------------------------
// Quota failure and reset detection (pure)

/// Whether a `turn/completed` turn failed on an exhausted usage limit
/// (adapter: `SdkResponseError` maps the SDK's `rate_limit` /
/// `billing_error` to `usageLimitExceeded` for subscription logins and
/// `rate_limit` to `rateLimitExceeded` for API accounts).
pub fn quota_failure(turn: &Value) -> bool {
    if turn.get("status").and_then(Value::as_str) != Some("failed") {
        return false;
    }
    let info = turn.pointer("/error/codexErrorInfo");
    let kind = match info {
        Some(Value::String(kind)) => Some(kind.as_str()),
        Some(Value::Object(map)) => map.keys().next().map(String::as_str),
        _ => None,
    };
    matches!(kind, Some("usageLimitExceeded" | "rateLimitExceeded"))
        || turn.pointer("/error/message").and_then(Value::as_str).is_some_and(quota_error_text)
}

/// The latest future reset among exhausted rows of an adapter
/// `account/rateLimits/read` result (`providerRateLimits.rows`: `status`
/// `rejected`, or `usedPercent` ≥ 100), unix seconds.
pub fn exhausted_reset_from_rate_limits(result: &Value, now: i64) -> Option<i64> {
    result
        .pointer("/providerRateLimits/rows")
        .and_then(Value::as_array)?
        .iter()
        .filter(|row| {
            row.get("status").and_then(Value::as_str) == Some("rejected")
                || row.get("usedPercent").and_then(Value::as_f64).is_some_and(|percent| percent >= 100.0)
        })
        .filter_map(|row| row.get("resetsAt").and_then(Value::as_i64))
        .filter(|reset| *reset > now)
        .max()
}

/// The latest future reset of the windows at 100 % of a Cutex Claude
/// account quota reading.
pub fn exhausted_reset_from_windows(windows: &[super::profile_quota_claude::StoredWindow], now: i64) -> Option<i64> {
    windows
        .iter()
        .filter(|window| window.used_percent >= 100.0)
        .filter_map(|window| window.reset_at)
        .filter(|reset| *reset > now)
        .max()
}

// ---------------------------------------------------------------------------
// Store

static STORE_LOCK: Mutex<()> = Mutex::new(());

pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn open_default() -> anyhow::Result<Self> {
        Ok(Self::at(crate::config::paths::runtime_dir()?.join("quota-continue/state-v1.json")))
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn read(&self) -> anyhow::Result<StateFile> {
        match std::fs::read(&self.path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(StateFile::default()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn snapshot(&self) -> anyhow::Result<StateFile> {
        let _guard = STORE_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        self.read()
    }

    /// Mutate under the process lock; written only when changed.
    pub fn with_state<T>(&self, mutate: impl FnOnce(&mut StateFile) -> T) -> anyhow::Result<T> {
        let _guard = STORE_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let mut state = self.read()?;
        let before = state.clone();
        let result = mutate(&mut state);
        if state != before {
            state.version = STATE_VERSION;
            if let Some(parent) = self.path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            crate::config::atomic::write_private_pretty_json_atomic(&self.path, &state, "quota auto-continue state")?;
        }
        Ok(result)
    }
}

/// Publish `cutex/quotaAutoContinue/changed` for `session`.
pub fn publish(state: &StateFile, session: &str, reason: &str) {
    let Ok(repository) = super::repository::management_v2_repository() else {
        return;
    };
    if let Err(error) = super::server::append_cutex_event(
        repository,
        super::services::HOST_EVENT_SESSION_ID,
        super::model::EventCorrelation::default(),
        CHANGED_EVENT,
        json!({
            "cutexSessionId": session,
            "reason": reason,
            "quotaAutoContinue": state.view(session),
            "hostId": crate::platform::host::current_host_name(),
        }),
    ) {
        eprintln!("quota auto-continue: could not publish event: {error:#}");
    }
}

/// Mutate the default store and publish `reason` when `mutate` reports a
/// change. Errors are logged (best-effort hooks).
pub fn update_default(session: &str, reason: &str, mutate: impl FnOnce(&mut StateFile) -> bool) -> bool {
    let store = match Store::open_default() {
        Ok(store) => store,
        Err(error) => {
            eprintln!("quota auto-continue: store unavailable: {error:#}");
            return false;
        }
    };
    match store.with_state(|state| mutate(state).then(|| state.clone())) {
        Ok(Some(state)) => {
            publish(&state, session, reason);
            true
        }
        Ok(None) => false,
        Err(error) => {
            eprintln!("quota auto-continue: {session}: {error:#}");
            false
        }
    }
}

/// Cancel `session`'s pending continuation (owner message, stop, offline,
/// retire, a completed turn). Cheap when nothing is pending: the store is
/// only read.
pub fn cancel_default(session: &str, reason: &str) -> bool {
    let pending = Store::open_default()
        .and_then(|store| store.snapshot())
        .is_ok_and(|state| state.sessions.get(session).is_some_and(|entry| entry.pending.is_some()));
    pending && update_default(session, "cancelled", |state| state.cancel(session, reason, Utc::now()))
}

/// A turn of `session` completed ([`StateFile::turn_completed`]). Cheap
/// when nothing is pending and no probe was counted: the store is only read.
pub fn completed_default(session: &str) -> bool {
    let relevant = Store::open_default()
        .and_then(|store| store.snapshot())
        .is_ok_and(|state| state.sessions.get(session).is_some_and(|entry| entry.pending.is_some() || entry.unknown_probes > 0));
    relevant && update_default(session, "cancelled", |state| state.turn_completed(session, Utc::now()))
}

/// Whether `session` is quota-held in the default store (asks scheduler).
pub fn held_default(session: &str) -> bool {
    Store::open_default().and_then(|store| store.snapshot()).is_ok_and(|state| state.quota_held(session))
}

/// Whether `session`'s auto-continue is on in the default store.
pub fn enabled_default(session: &str) -> bool {
    Store::open_default().and_then(|store| store.snapshot()).map(|state| state.enabled(session)).unwrap_or(true)
}

/// Owner requests on a session that end a pending continuation.
pub fn cancels_pending(method: &str) -> Option<&'static str> {
    match method {
        "cutex/userInput/submit" | "cutex/userInput/queue/flush" | "cutex/userInput/queue/steer" => Some("ownerMessage"),
        "cutex/turn/stop" => Some("ownerStop"),
        "cutex/runtime/offline" | "cutex/runtime/close" => Some("runtimeOffline"),
        "cutex/session/retire" | "cutex/session/delete" => Some("archived"),
        _ => None,
    }
}

/// The continuation as a `cutex/userInput/submit` (owner input of origin
/// `automation`, never starting an offline runtime).
fn submit_request(reset_at: i64) -> super::server::ValidatedCutexRequest {
    let client_id = format!("quota-continue-{reset_at}-{}", uuid::Uuid::new_v4().simple());
    super::server::ValidatedCutexRequest {
        request_id: client_id.clone(),
        method: "cutex/userInput/submit".into(),
        params: json!({
            "clientUserMessageId": client_id,
            "origin": {"kind": "automation", "clientId": CLIENT_ID},
            "strategy": "auto",
            "input": [{"type": "text", "text": CONTINUE_TEXT, "text_elements": []}],
        }),
    }
}

/// Send the continuation through the ordinary submit path (management
/// process). Returns the submit disposition.
pub fn send(context: crate::management::server::ManagementRequestContext, session: &str, reset_at: i64) -> anyhow::Result<String> {
    let repository = super::repository::management_v2_repository()?;
    let registry = (context.load_registry)()?;
    let resource = super::session::session_resource_including_hidden(session, &registry, context.load_runtime_status, repository)?
        .ok_or_else(|| anyhow::anyhow!("session not found"))?;
    let request = submit_request(reset_at);
    let result = super::offline_submit::dispatch(
        repository,
        context,
        Some(crate::management::control_plane::HumanManagementPrincipal::authenticated()),
        session,
        &resource,
        &request,
    )
    .map_err(|error| anyhow::anyhow!("{}: {}", error.code, error.message))?;
    Ok(result.get("disposition").and_then(Value::as_str).unwrap_or("unknown").to_string())
}

// ---------------------------------------------------------------------------
// HTTP

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    Collection,
    Item(String),
    Pending(String),
}

fn valid_session_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 200 && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

pub fn parse_path(path: &str) -> Option<Route> {
    if path == PATH {
        return Some(Route::Collection);
    }
    let rest = path.strip_prefix("/v2/quota-auto-continue/")?;
    if let Some(id) = rest.strip_suffix("/pending") {
        return valid_session_id(id).then(|| Route::Pending(id.to_string()));
    }
    valid_session_id(rest).then(|| Route::Item(rest.to_string()))
}

pub fn is_path(path: &str) -> bool {
    path == PATH || path.starts_with("/v2/quota-auto-continue/")
}

fn query_session(path: &str) -> Result<Option<String>, String> {
    let Some(query) = path.split_once('?').map(|(_, query)| query) else {
        return Ok(None);
    };
    let mut session = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "sessionId" if valid_session_id(&value) => session = Some(value.into_owned()),
            "sessionId" => return Err("invalid sessionId".into()),
            other => return Err(format!("unknown query parameter {other}")),
        }
    }
    Ok(session)
}

fn session_known(session: &str) -> bool {
    crate::session::store::load_cutex_session_store_shared()
        .map(|store| store.sessions.contains_key(session))
        .unwrap_or(false)
}

fn error(stream: &mut TcpStream, status: u16, code: &str, message: &str, retryable: bool) -> anyhow::Result<()> {
    let reason = match status {
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Service Unavailable",
    };
    super::server::write_v2_error(stream, status, reason, code, message, retryable, json!({}))
}

/// Socket entry point; the caller has already verified the root credential.
pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let store = match Store::open_default() {
        Ok(store) => store,
        Err(e) => return error(stream, 503, "quota_continue_unavailable", &format!("{e:#}"), true),
    };
    let host = crate::platform::host::current_host_name();
    match (request.method.as_str(), parse_path(path)) {
        ("GET", Some(Route::Collection)) => {
            let session = match query_session(&request.path) {
                Ok(session) => session,
                Err(message) => return error(stream, 400, "invalid_request", &message, false),
            };
            match store.snapshot() {
                Ok(state) => {
                    let sessions: Vec<Value> = match session {
                        Some(session) => vec![state.view(&session)],
                        None => state.sessions.keys().map(|session| state.view(session)).collect(),
                    };
                    write_json_response(stream, 200, "OK", &json!({"hostId": host, "sessions": sessions}))
                }
                Err(e) => error(stream, 503, "quota_continue_unavailable", &format!("{e:#}"), true),
            }
        }
        ("PATCH", Some(Route::Item(session))) => {
            let enabled = serde_json::from_slice::<Value>(&request.body).ok().and_then(|body| {
                let object = body.as_object()?;
                (object.len() == 1).then(|| object.get("enabled")?.as_bool()).flatten()
            });
            let Some(enabled) = enabled else {
                return error(stream, 400, "invalid_request", "body must be {\"enabled\": true|false}", false);
            };
            if !session_known(&session) {
                return error(stream, 404, "session_not_found", "no such agent on this host", false);
            }
            let reason = if enabled { "enabled" } else { "disabled" };
            match store.with_state(|state| (state.set_enabled(&session, enabled, Utc::now()), state.clone())) {
                Ok((changed, state)) => {
                    if changed {
                        publish(&state, &session, reason);
                    }
                    write_json_response(stream, 200, "OK", &json!({"quotaAutoContinue": state.view(&session)}))
                }
                Err(e) => error(stream, 503, "quota_continue_unavailable", &format!("{e:#}"), true),
            }
        }
        ("DELETE", Some(Route::Pending(session))) => {
            match store.with_state(|state| (state.cancel(&session, "owner", Utc::now()), state.clone())) {
                Ok((changed, state)) => {
                    if changed {
                        publish(&state, &session, "cancelled");
                    }
                    write_json_response(stream, 200, "OK", &json!({"quotaAutoContinue": state.view(&session)}))
                }
                Err(e) => error(stream, 503, "quota_continue_unavailable", &format!("{e:#}"), true),
            }
        }
        (_, Some(_)) => error(stream, 405, "method_not_allowed", "method not allowed for this route", false),
        (_, None) => error(stream, 404, "not_found", "unknown quota auto-continue route", false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: &str = "cutex.11111111-2222-3333-4444-555555555555";

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).single().unwrap()
    }

    #[test]
    fn quota_failures_are_usage_and_rate_limits_only() {
        assert!(quota_failure(&json!({"status": "failed", "error": {"codexErrorInfo": "usageLimitExceeded"}})));
        assert!(quota_failure(&json!({"status": "failed", "error": {"codexErrorInfo": "rateLimitExceeded"}})));
        assert!(quota_failure(&json!({"status": "failed", "error": {"codexErrorInfo": {"usageLimitExceeded": {}}}})));
        assert!(!quota_failure(&json!({"status": "failed", "error": {"codexErrorInfo": "other"}})));
        assert!(!quota_failure(&json!({"status": "completed", "error": {"codexErrorInfo": "usageLimitExceeded"}})));
        assert!(!quota_failure(&json!({"status": "interrupted", "error": null})));
    }

    #[test]
    fn reset_comes_from_exhausted_rows_or_windows() {
        let now = 1_000;
        let limits = json!({"providerRateLimits": {"version": 1, "rows": [
            {"id": "five_hour", "status": "rejected", "usedPercent": null, "resetsAt": 5_000},
            {"id": "seven_day", "status": "allowed", "usedPercent": 40.0, "resetsAt": 90_000},
            {"id": "seven_day_opus", "status": "observed", "usedPercent": 100.0, "resetsAt": 7_000},
            {"id": "old", "status": "rejected", "resetsAt": 500},
        ]}});
        assert_eq!(exhausted_reset_from_rate_limits(&limits, now), Some(7_000));
        assert_eq!(exhausted_reset_from_rate_limits(&json!({"providerRateLimits": {"rows": []}}), now), None);
        assert_eq!(exhausted_reset_from_rate_limits(&json!({}), now), None);
        use super::super::profile_quota_claude::StoredWindow;
        let windows = vec![
            StoredWindow { label: "5h".into(), used_percent: 100.0, reset_at: Some(4_000) },
            StoredWindow { label: "7d".into(), used_percent: 99.0, reset_at: Some(80_000) },
        ];
        assert_eq!(exhausted_reset_from_windows(&windows, now), Some(4_000));
        assert_eq!(exhausted_reset_from_windows(&windows[1..], now), None);
    }

    #[test]
    fn one_continuation_per_window_with_jitter() {
        let mut state = StateFile::default();
        let now = at(1_000);
        assert!(state.schedule(S, 2_000, Some("default".into()), Some("t1".into()), 3, now));
        let pending = state.sessions[S].pending.clone().unwrap();
        let jitter = jitter_secs(S);
        assert!((30..120).contains(&jitter));
        assert_eq!(crate::asks::parse_time(&pending.at).unwrap().timestamp(), 2_000 + jitter);
        // A second failure in the same window changes nothing.
        assert!(!state.schedule(S, 2_000, Some("default".into()), Some("t2".into()), 3, now));
        assert!(state.due(at(2_000)).is_empty());
        assert_eq!(state.next_at().unwrap().timestamp(), 2_000 + jitter);
        let due = at(2_000 + jitter);
        assert_eq!(state.due(due), vec![S.to_string()]);
        let taken = state.take_due(S, due).unwrap();
        assert_eq!(taken.reset_at, 2_000);
        assert!(state.take_due(S, due).is_none());
        // The continuation hit the limit again in the same (stale) window:
        // no second continuation for it, an estimated probe instead.
        assert!(state.schedule(S, 2_000, None, None, 3, due));
        let probe = state.sessions[S].pending.clone().unwrap();
        assert!(probe.estimated);
        assert_eq!(probe.reset_at, due.timestamp() + estimate_delay_secs(0));
        assert_eq!(state.sessions[S].last.as_ref().unwrap().reason.as_deref(), Some("windowAlreadyContinued"));
        // The next window schedules again.
        assert!(state.schedule(S, 20_000, None, None, 3, due));
        assert!(state.sessions[S].pending.is_some());
        let view = state.view(S);
        assert_eq!(view["enabled"], json!(true));
        assert_eq!(view["pending"]["resetAt"], json!("1970-01-01T05:33:20Z"));
    }

    #[test]
    fn claude_r5_error_texts_are_quota_failures() {
        // claude-r5 / r7 report `other` (owner report 2026-10-14).
        let r5 = json!({"status": "failed", "error": {"codexErrorInfo": "other", "message":
            "Official Claude returned an error (rate_limit). You may submit a fresh instruction; the failed input is not automatically retried."}});
        assert!(quota_failure(&r5));
        assert!(quota_error_text("Official Claude returned an error (billing_error). You may submit a fresh instruction; the failed input is not automatically retried."));
        assert!(quota_error_text("Claude API account 'work' (api.example.com) rejected the request (rate_limit)."));
        assert!(!quota_error_text("Official Claude returned an error (server_error). You may submit a fresh instruction; the failed input is not automatically retried."));
        assert!(!quota_error_text("Claude API account 'work' (api.example.com) rejected the request (authentication_failed). Check the API key and base URL."));
        assert!(!quota_error_text("provider said: Official Claude returned an error (rate_limit)."));
        assert!(!quota_failure(&json!({"status": "completed", "error": r5["error"].clone()})));
    }

    #[test]
    fn hold_then_known_or_estimated_reset() {
        let now = at(10_000);
        // Held at the failure: estimated until the reset lookup ran.
        let mut state = StateFile::default();
        assert!(state.hold(S, Some("t1".into()), 2, now));
        assert!(state.quota_held(S));
        let held = state.sessions[S].pending.clone().unwrap();
        assert!(held.estimated);
        assert_eq!(held.reset_at, 10_000 + estimate_delay_secs(0));
        // A second hold (repeated turn/completed) changes nothing.
        assert!(!state.hold(S, Some("t1".into()), 2, now));
        // The known reset replaces it.
        assert!(state.schedule(S, 50_000, Some("acct".into()), Some("t1".into()), 2, now));
        let known = state.sessions[S].pending.clone().unwrap();
        assert!(!known.estimated);
        assert_eq!(known.reset_at, 50_000);
        assert_eq!(state.view(S)["pending"]["estimated"], json!(false));

        // Unknown reset: the hold is the probe; repeated lookups don't count.
        let mut state = StateFile::default();
        state.hold(S, Some("t1".into()), 2, now);
        assert!(state.schedule_estimated(S, Some("acct".into()), Some("t1".into()), 2, now));
        assert!(state.schedule_estimated(S, Some("acct".into()), Some("t1".into()), 2, now));
        assert_eq!(state.sessions[S].pending.as_ref().unwrap().reset_at, held.reset_at);
        assert_eq!(state.sessions[S].unknown_probes, 0);
        // Probes back off (fired count) and end after MAX_ESTIMATED_PROBES.
        let mut time = now;
        for probe in 0..MAX_ESTIMATED_PROBES {
            let pending = state.sessions[S].pending.clone().unwrap();
            assert_eq!(pending.reset_at - time.timestamp(), estimate_delay_secs(probe));
            time = crate::asks::parse_time(&pending.at).unwrap();
            assert!(state.take_due(S, time).is_some());
            assert!(!state.quota_held(S));
            assert_eq!(state.sessions[S].unknown_probes, probe + 1);
            // The probe failed again with no known reset.
            let turn = Some(format!("p{probe}"));
            let held = state.hold(S, turn.clone(), 2, time);
            assert_eq!(held, probe + 1 < MAX_ESTIMATED_PROBES);
            state.schedule_estimated(S, None, turn, 2, time);
        }
        assert!(state.sessions[S].pending.is_none());
        assert_eq!(state.sessions[S].last.as_ref().unwrap().reason.as_deref(), Some("resetUnknown"));
        assert_eq!(estimate_delay_secs(0), 1800);
        assert_eq!(estimate_delay_secs(7), ESTIMATE_MAX_SECS);
        // A completed turn resets the count.
        assert!(state.turn_completed(S, time));
        assert_eq!(state.sessions.get(S).map(|entry| entry.unknown_probes).unwrap_or(0), 0);
        assert!(state.hold(S, None, 2, time));
        assert!(state.turn_completed(S, time));
        assert!(!state.quota_held(S));
        assert!(!state.turn_completed(S, time));
    }

    #[test]
    fn cancel_and_switch() {
        let mut state = StateFile::default();
        let now = at(1_000);
        assert!(!state.cancel(S, "ownerMessage", now));
        state.schedule(S, 2_000, None, None, 1, now);
        assert!(state.cancel(S, "ownerMessage", now));
        assert_eq!(state.sessions[S].last.as_ref().unwrap().result, "cancelled");
        assert!(!state.cancel(S, "ownerMessage", now));
        // Off: pending cancelled, nothing schedules; on again: the default.
        state.schedule(S, 3_000, None, None, 1, now);
        assert!(state.set_enabled(S, false, now));
        assert!(state.sessions[S].pending.is_none());
        assert!(!state.schedule(S, 4_000, None, None, 1, now));
        assert!(!state.schedule_estimated(S, None, None, 1, now));
        assert!(!state.hold(S, None, 1, now));
        assert_eq!(state.view(S)["enabled"], json!(false));
        assert!(!state.set_enabled(S, false, now));
        assert!(state.set_enabled(S, true, now));
        assert_eq!(state.view(S)["enabled"], json!(true));
        // A disabled agent's due entry is never taken.
        let mut other = StateFile::default();
        other.schedule(S, 2_000, None, None, 1, now);
        other.sessions.get_mut(S).unwrap().disabled = true;
        assert!(other.take_due(S, at(9_999)).is_none());
    }

    #[test]
    fn default_view_and_routes() {
        let state = StateFile::default();
        let view = state.view(S);
        assert_eq!(view, json!({"cutexSessionId": S, "enabled": true, "pending": null, "last": null}));
        assert_eq!(parse_path(PATH), Some(Route::Collection));
        assert_eq!(parse_path(&format!("{PATH}/{S}")), Some(Route::Item(S.into())));
        assert_eq!(parse_path(&format!("{PATH}/{S}/pending")), Some(Route::Pending(S.into())));
        assert_eq!(parse_path(&format!("{PATH}/bad id")), None);
        assert_eq!(query_session(&format!("{PATH}?sessionId={S}")), Ok(Some(S.into())));
        assert!(query_session(&format!("{PATH}?limit=1")).is_err());
        assert_eq!(cancels_pending("cutex/userInput/submit"), Some("ownerMessage"));
        assert_eq!(cancels_pending("cutex/runtime/close"), Some("runtimeOffline"));
        assert_eq!(cancels_pending("cutex/asks/resume"), None);
        let request = submit_request(2_000);
        assert_eq!(request.params["origin"]["kind"], json!("automation"));
        assert!(request.params.get("startIfOffline").is_none());
    }

    #[test]
    fn views_match_the_exported_schema() {
        use super::super::schema_export::assert_valid;
        let mut state = StateFile::default();
        assert_valid("quota-auto-continue", "quotaAutoContinue", &state.view(S));
        state.schedule(S, 2_000, Some("default".into()), Some("turn".into()), 1, at(1_000));
        let view = state.view(S);
        assert_valid("quota-auto-continue", "quotaAutoContinue", &view);
        let mut estimated = StateFile::default();
        estimated.hold(S, None, 1, at(1_000));
        assert_valid("quota-auto-continue", "quotaAutoContinue", &estimated.view(S));
        assert_valid("quota-auto-continue", "quotaAutoContinueItem", &json!({"quotaAutoContinue": view}));
        assert_valid("quota-auto-continue", "quotaAutoContinueList", &json!({"hostId": "h", "sessions": [view]}));
        state.cancel(S, "owner", at(1_500));
        assert_valid("quota-auto-continue", "quotaAutoContinueChanged",
            &json!({"cutexSessionId": S, "reason": "cancelled", "quotaAutoContinue": state.view(S), "hostId": "h"}));
    }

    #[test]
    fn store_persists_only_changes() {
        let dir = std::env::temp_dir().join(format!("cutex-quota-continue-{}", uuid::Uuid::new_v4()));
        let store = Store::at(dir.join("state-v1.json"));
        store.with_state(|state| state.cancel(S, "x", Utc::now())).unwrap();
        assert!(!dir.exists(), "a no-op must not create the store");
        store.with_state(|state| state.schedule(S, 2_000, None, None, 1, at(1_000))).unwrap();
        assert!(store.snapshot().unwrap().sessions[S].pending.is_some());
        assert_eq!(store.snapshot().unwrap().version, STATE_VERSION);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
