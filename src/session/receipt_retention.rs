//! Explicit-launch receipt retention (cutex-desktop
//! `docs/research/session-store-sqlite.md` §5 Phase 0).
//!
//! `explicit_launch_receipts` grows by one entry per launch / migration and
//! was never pruned; on a busy host it is >90 % of `cutex-sessions.json`.
//! This module removes receipts nothing needs any more and leaves a small
//! tombstone per removed action, so replaying or querying that action fails
//! with `explicit_launch_action_pruned` instead of being treated as new.
//!
//! A receipt is kept when any of these holds (first match is reported):
//!
//! 1. its action is some session's `explicit_launch.migration_action_id`;
//! 2. its session still exists and
//!    - runtime: stage is not `ready`, or it carries an error (recovery and
//!      status still need it);
//!    - runtime: it is the newest `ready` receipt (highest
//!      `expected_generation`) of its (session, native id, native home):
//!      `saved_runtime_job` reuses its Job MCP review and task lineage
//!      accepts it as launch provenance;
//!    - runtime: it belongs to the session's live or last occurrence
//!      (current / last runtime agent id, launch claim id or runtime binding):
//!      ingress fences, attach / stop and registration look it up;
//!    - activation: its contract is the session's current contract (task
//!      lineage reads its `committed_at`);
//!    - bootstrap: always (no timestamp; the later online step needs it);
//!    - maintenance (Linux): unexpired or not `applied` (an applied,
//!      expired receipt the contract no longer names is history only: a
//!      migrated record has `explicit_launch` set and a Host backend, which
//!      `review_maintenance` / `apply_maintenance` already refuse);
//!    - its timestamp (runtime `updated_at`, activation `committed_at`,
//!      maintenance `expires_at_unix`) is within the retention window, or
//!      cannot be parsed.
//!
//! Receipts of sessions that no longer exist are pruned (unless rule 1).
//! Tombstones are kept [`TOMBSTONE_RETENTION_DAYS`] after pruning.
//!
//! Rollback: the tombstones live in a new top-level map
//! (`explicitLaunchTombstones`). Older cutex (r215 and before) does not deny
//! unknown store fields: it ignores the map and drops it on its next save, so
//! a pruned action id then reads as "not found" again there. The store format
//! is otherwise unchanged.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::agent_management::{
    ExplicitLaunchActionReceipt, StockRuntimeReceipt, StockRuntimeStage,
};
use crate::session::model::{CutexSessionRecord, CutexSessionStore};

/// How long a tombstone outlives its receipt. Clients retry or poll an
/// action within minutes to days; 90 days covers a forgotten status query.
pub const TOMBSTONE_RETENTION_DAYS: i64 = 90;
/// Error code prefix for a pruned action.
pub const PRUNED_ERROR: &str = "explicit_launch_action_pruned";
/// Run at management start and then this often.
pub const PRUNE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

/// What is left of a pruned explicit-launch action. Compact on purpose
/// (~200 bytes); no review data.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplicitLaunchTombstone {
    /// Receipt kind: `runtime`, `activation`, `bootstrap`, `maintenance`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Runtime stage / maintenance phase when pruned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    /// sha256 of the pruned receipt's JSON (audit / matching a backup).
    pub sha256: String,
    /// RFC 3339, UTC, seconds.
    pub pruned_at: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum KeepReason {
    MigrationReference,
    NotTerminal,
    NewestReady,
    LiveRuntime,
    CurrentActivation,
    Bootstrap,
    MaintenanceOpen,
    Recent,
    UnknownAge,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceiptPruneReport {
    pub receipts_before: usize,
    pub kept: usize,
    pub pruned: usize,
    /// Of `pruned`: receipts whose session no longer exists.
    pub pruned_orphaned: usize,
    pub tombstones_added: usize,
    pub tombstones_expired: usize,
    /// Tombstones dropped because a receipt with that key exists again.
    pub tombstones_superseded: usize,
}

impl ReceiptPruneReport {
    pub fn changed(&self) -> bool {
        self.pruned > 0 || self.tombstones_expired > 0 || self.tombstones_superseded > 0
    }
}

/// The changes one prune would make (computed on a read-only store).
#[derive(Debug, Default)]
pub struct PrunePlan {
    pub keep: BTreeMap<String, KeepReason>,
    pub prune: BTreeMap<String, (ExplicitLaunchTombstone, bool)>,
    pub expire_tombstones: Vec<String>,
    pub superseded_tombstones: Vec<String>,
}

impl PrunePlan {
    pub fn is_empty(&self) -> bool {
        self.prune.is_empty()
            && self.expire_tombstones.is_empty()
            && self.superseded_tombstones.is_empty()
    }
}

fn receipt_kind(receipt: &ExplicitLaunchActionReceipt) -> &'static str {
    match receipt {
        ExplicitLaunchActionReceipt::Bootstrap(_) => "bootstrap",
        ExplicitLaunchActionReceipt::Activation(_) => "activation",
        ExplicitLaunchActionReceipt::Runtime(_) => "runtime",
        #[cfg(target_os = "linux")]
        ExplicitLaunchActionReceipt::Maintenance(_) => "maintenance",
    }
}

fn receipt_session(receipt: &ExplicitLaunchActionReceipt) -> &str {
    match receipt {
        ExplicitLaunchActionReceipt::Bootstrap(r) => r.cutex_session_id.as_str(),
        ExplicitLaunchActionReceipt::Activation(r) => r.review.subject.cutex_session_id.as_str(),
        ExplicitLaunchActionReceipt::Runtime(r) => r.review.subject.cutex_session_id.as_str(),
        #[cfg(target_os = "linux")]
        ExplicitLaunchActionReceipt::Maintenance(r) => r.review.subject.cutex_session_id.as_str(),
    }
}

fn receipt_stage(receipt: &ExplicitLaunchActionReceipt) -> Option<String> {
    let value = match receipt {
        ExplicitLaunchActionReceipt::Runtime(r) => serde_json::to_value(&r.stage).ok(),
        #[cfg(target_os = "linux")]
        ExplicitLaunchActionReceipt::Maintenance(r) => serde_json::to_value(r.phase).ok(),
        _ => None,
    };
    value.and_then(|value| value.as_str().map(str::to_owned))
}

fn receipt_sha256(receipt: &ExplicitLaunchActionReceipt) -> String {
    use sha2::Digest;
    let bytes = serde_json::to_vec(receipt).unwrap_or_default();
    format!("{:x}", sha2::Sha256::digest(bytes))
}

fn parse_time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

fn by_age(at: Option<DateTime<Utc>>, cutoff: DateTime<Utc>) -> Option<KeepReason> {
    match at {
        None => Some(KeepReason::UnknownAge),
        Some(at) if at >= cutoff => Some(KeepReason::Recent),
        Some(_) => None,
    }
}

fn live_runtime(record: &CutexSessionRecord, receipt: &StockRuntimeReceipt) -> bool {
    let agent = Some(receipt.runtime_agent_id.as_str());
    record.current_runtime_agent_id.as_deref() == agent
        || record.last_runtime_agent_id.as_deref() == agent
        || record.app_server_launch_claim_id.as_deref() == Some(receipt.claim_id.as_str())
        || (record.app_server_runtime.is_some()
            && receipt.binding.as_ref() == record.app_server_runtime.as_ref())
}

/// Decide, without changing anything, which receipts stay.
pub fn plan(store: &CutexSessionStore, now: DateTime<Utc>, retention_days: u32) -> PrunePlan {
    let cutoff = now - chrono::Duration::days(i64::from(retention_days));
    let referenced: std::collections::BTreeSet<&str> = store
        .sessions
        .values()
        .filter_map(|record| {
            record
                .explicit_launch
                .as_ref()?
                .migration_action_id
                .as_ref()
                .map(|action| action.as_str())
        })
        .collect();
    let mut newest_ready: HashMap<(&str, &str, &Path), u64> = HashMap::new();
    for receipt in store.explicit_launch_receipts.values() {
        if let ExplicitLaunchActionReceipt::Runtime(r) = receipt {
            if r.stage == StockRuntimeStage::Ready {
                let key = (
                    r.review.subject.cutex_session_id.as_str(),
                    r.review.contract.native_id.as_str(),
                    r.review.contract.native_home.as_path(),
                );
                let entry = newest_ready.entry(key).or_insert(r.expected_generation);
                *entry = (*entry).max(r.expected_generation);
            }
        }
    }
    let pruned_at = now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut result = PrunePlan::default();
    for (key, receipt) in &store.explicit_launch_receipts {
        let reason = if referenced.contains(key.as_str()) {
            Some(KeepReason::MigrationReference)
        } else if let Some(record) = store.sessions.get(receipt_session(receipt)) {
            keep_reason(record, receipt, &newest_ready, now, cutoff)
        } else {
            None
        };
        match reason {
            Some(reason) => {
                result.keep.insert(key.clone(), reason);
            }
            None => {
                let orphaned = !store.sessions.contains_key(receipt_session(receipt));
                result.prune.insert(
                    key.clone(),
                    (
                        ExplicitLaunchTombstone {
                            kind: receipt_kind(receipt).to_owned(),
                            session: Some(receipt_session(receipt).to_owned()),
                            stage: receipt_stage(receipt),
                            sha256: receipt_sha256(receipt),
                            pruned_at: pruned_at.clone(),
                        },
                        orphaned,
                    ),
                );
            }
        }
    }
    let tombstone_cutoff = now - chrono::Duration::days(TOMBSTONE_RETENTION_DAYS);
    for (key, tombstone) in &store.explicit_launch_tombstones {
        if store.explicit_launch_receipts.contains_key(key) {
            result.superseded_tombstones.push(key.clone());
        } else if parse_time(&tombstone.pruned_at).is_some_and(|at| at < tombstone_cutoff) {
            result.expire_tombstones.push(key.clone());
        }
    }
    result
}

#[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
fn keep_reason(
    record: &CutexSessionRecord,
    receipt: &ExplicitLaunchActionReceipt,
    newest_ready: &HashMap<(&str, &str, &Path), u64>,
    now: DateTime<Utc>,
    cutoff: DateTime<Utc>,
) -> Option<KeepReason> {
    match receipt {
        ExplicitLaunchActionReceipt::Runtime(r) => {
            if r.stage != StockRuntimeStage::Ready || r.error.is_some() {
                return Some(KeepReason::NotTerminal);
            }
            let key = (
                r.review.subject.cutex_session_id.as_str(),
                r.review.contract.native_id.as_str(),
                r.review.contract.native_home.as_path(),
            );
            if newest_ready.get(&key) == Some(&r.expected_generation) {
                return Some(KeepReason::NewestReady);
            }
            if live_runtime(record, r) {
                return Some(KeepReason::LiveRuntime);
            }
            by_age(parse_time(&r.updated_at), cutoff)
        }
        ExplicitLaunchActionReceipt::Activation(a) => {
            if record.explicit_launch.as_ref() == Some(&a.review.contract) {
                return Some(KeepReason::CurrentActivation);
            }
            by_age(parse_time(&a.committed_at), cutoff)
        }
        ExplicitLaunchActionReceipt::Bootstrap(_) => Some(KeepReason::Bootstrap),
        #[cfg(target_os = "linux")]
        ExplicitLaunchActionReceipt::Maintenance(m) => {
            if m.review.expires_at_unix > now.timestamp()
                || m.phase != crate::agent_management::MaintenancePhase::Applied
            {
                Some(KeepReason::MaintenanceOpen)
            } else {
                by_age(
                    DateTime::from_timestamp(m.review.expires_at_unix, 0),
                    cutoff,
                )
            }
        }
    }
}

/// Apply a plan made on this same store state.
pub fn apply(store: &mut CutexSessionStore, plan: PrunePlan) -> ReceiptPruneReport {
    let mut report = ReceiptPruneReport {
        receipts_before: store.explicit_launch_receipts.len(),
        kept: plan.keep.len(),
        ..Default::default()
    };
    for key in plan.superseded_tombstones {
        if store.explicit_launch_tombstones.remove(&key).is_some() {
            report.tombstones_superseded += 1;
        }
    }
    for key in plan.expire_tombstones {
        if store.explicit_launch_tombstones.remove(&key).is_some() {
            report.tombstones_expired += 1;
        }
    }
    for (key, (tombstone, orphaned)) in plan.prune {
        if store.explicit_launch_receipts.remove(&key).is_some() {
            report.pruned += 1;
            report.pruned_orphaned += usize::from(orphaned);
            store.explicit_launch_tombstones.insert(key, tombstone);
            report.tombstones_added += 1;
        }
    }
    report
}

/// Plan and apply in memory (no I/O).
pub fn prune(
    store: &mut CutexSessionStore,
    now: DateTime<Utc>,
    retention_days: u32,
) -> ReceiptPruneReport {
    let plan = plan(store, now, retention_days);
    apply(store, plan)
}

/// Prune the store at `path` under the normal session store lock. Nothing
/// is written (and no lock taken) when there is nothing to prune, so it is
/// cheap to repeat and idempotent.
pub fn prune_store_at(
    path: &Path,
    retention_days: u32,
    now: DateTime<Utc>,
) -> anyhow::Result<ReceiptPruneReport> {
    let pending = crate::session::store::with_cutex_session_store_at(path, |store| {
        !plan(store, now, retention_days).is_empty()
    })?;
    if !pending {
        return Ok(ReceiptPruneReport::default());
    }
    crate::session::store::with_locked_session_store(path, |store| {
        let report = prune(store, now, retention_days);
        if report.changed() {
            crate::session::store::save_locked_session_store(path, store)?;
        }
        Ok(report)
    })
}

/// [`prune_store_at`] on the default store with the configured retention.
pub fn prune_default_store() -> anyhow::Result<ReceiptPruneReport> {
    let settings = crate::config::session_store::current();
    prune_store_at(
        &crate::session::store::cutex_sessions_path()?,
        settings.receipt_retention_days,
        Utc::now(),
    )
}

/// `Err(explicit_launch_action_pruned …)` when `key` has no receipt but a
/// tombstone: the action existed and was pruned, so it must not be replayed
/// or re-created under the same id.
pub fn ensure_action_not_pruned(store: &CutexSessionStore, key: &str) -> anyhow::Result<()> {
    if store.explicit_launch_receipts.contains_key(key) {
        return Ok(());
    }
    if let Some(tombstone) = store.explicit_launch_tombstones.get(key) {
        anyhow::bail!(
            "{PRUNED_ERROR}: {} action {key} finished and its receipt was removed by session store retention at {}; it cannot be replayed or queried, start a new action",
            tombstone.kind,
            tombstone.pruned_at
        );
    }
    Ok(())
}

/// The receipt of `key`, `None` for an unknown action, or the pruned error.
pub fn explicit_launch_receipt<'a>(
    store: &'a CutexSessionStore,
    key: &str,
) -> anyhow::Result<Option<&'a ExplicitLaunchActionReceipt>> {
    ensure_action_not_pruned(store, key)?;
    Ok(store.explicit_launch_receipts.get(key))
}

#[cfg(test)]
mod tests;
