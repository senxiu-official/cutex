//! Limits and budgets (asks design §8.3) with the owner's defaults.
//!
//! Budgets are kept per project or independent creator tree (the domain).
//! Over the auto-turn budget, ANSWER/QUESTION events degrade to passive and
//! the owner is notified.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::store::{AskState, AskStore};
use super::{now_rfc3339, parse_time};

/// Default of [`AskLimits::stop_offline_grace_secs`].
pub const STOP_OFFLINE_GRACE_SECS: u64 = 300;

/// Configurable limits (`config.json` `asks.limits`, all optional).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AskLimits {
    /// Live (not archived) children per agent.
    pub children_per_agent: usize,
    /// Creation depth: a child may create, a grandchild may not.
    pub creation_depth: u32,
    /// Live children per project / independent tree.
    pub children_per_domain: usize,
    /// Agent-started online runtimes per project / tree.
    pub agent_started_online_per_domain: usize,
    /// Auto-started turns per hour per project / tree.
    pub auto_turns_per_hour: usize,
    /// Seconds a pending stop (stop / abandon of a running delegation)
    /// waits for a target that is offline and not reconnectable before the
    /// attempt ends `interrupted(stopped)` without the runtime. An archived
    /// target settles at once.
    pub stop_offline_grace_secs: u64,
}

impl Default for AskLimits {
    fn default() -> Self {
        Self {
            children_per_agent: 4,
            creation_depth: 2,
            children_per_domain: 12,
            agent_started_online_per_domain: 6,
            auto_turns_per_hour: 60,
            stop_offline_grace_secs: STOP_OFFLINE_GRACE_SECS,
        }
    }
}

impl AskLimits {
    /// The configured limits (`config.json` → `asks.limits`), defaults
    /// otherwise.
    pub fn load() -> Self {
        let Ok(path) = crate::config::paths::config_dir().map(|dir| dir.join("config.json")) else {
            return Self::default();
        };
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|config| config.pointer("/asks/limits").cloned())
            .and_then(|limits| serde_json::from_value(limits).ok())
            .unwrap_or_default()
    }
}

/// Auto-started turns of a budget key in the last hour.
pub fn auto_turns_last_hour(state: &AskState, key: &str, now: DateTime<Utc>) -> usize {
    let hour_ago = now - chrono::Duration::hours(1);
    state
        .auto_turns
        .get(key)
        .map(|times| times.iter().filter(|time| parse_time(time).is_some_and(|time| time >= hour_ago)).count())
        .unwrap_or(0)
}

/// Take one auto-started turn from `key`'s budget. Over budget: degrade the
/// requester's pending events to passive, notify the owner, return false.
pub fn take_auto_turn(store: &AskStore, key: &str, requester: &str, limit: usize, now: DateTime<Utc>) -> anyhow::Result<bool> {
    store.with_state(|state| {
        if auto_turns_last_hour(state, key, now) >= limit {
            let degraded = super::delivery::degrade_to_passive(state, requester, now);
            if degraded > 0 {
                state.notice(
                    "cutex/ask/budgetExceeded",
                    vec![crate::management::v2::services::HOST_EVENT_SESSION_ID.to_string(), requester.to_string()],
                    json!({
                        "budget": "autoTurnsPerHour",
                        "key": key,
                        "limit": limit,
                        "requester": requester,
                        "degradedToPassive": degraded,
                    }),
                );
            }
            return false;
        }
        let hour_ago = now - chrono::Duration::hours(1);
        let times = state.auto_turns.entry(key.to_string()).or_default();
        times.retain(|time| parse_time(time).is_some_and(|time| time >= hour_ago));
        times.push(now_rfc3339(now));
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::store::tests::{new_ask, temp_store, REQUESTER};

    /// Acceptance §12.3 item 10 (part): an ask storm over the auto-turn
    /// budget degrades to passive and notifies the owner.
    #[test]
    fn over_budget_degrades_to_passive() {
        let (store, dir) = temp_store("budget");
        let now = Utc::now();
        assert!(take_auto_turn(&store, "project:alpha", REQUESTER, 2, now).unwrap());
        assert!(take_auto_turn(&store, "project:alpha", REQUESTER, 2, now).unwrap());
        store
            .with_state(|state| {
                let (ask, _) = state.create_ask(new_ask("a", "x"), now).unwrap();
                crate::asks::delivery::record_answer_delivery(state, &ask, "ans-1", now);
            })
            .unwrap();
        assert!(!take_auto_turn(&store, "project:alpha", REQUESTER, 2, now).unwrap());
        let state = store.snapshot().unwrap();
        assert!(state.deliveries.values().all(|delivery| delivery.passive));
        assert!(state.notices.iter().any(|notice| notice.method == "cutex/ask/budgetExceeded"));
        // An hour later the budget is back.
        assert!(take_auto_turn(&store, "project:alpha", REQUESTER, 2, now + chrono::Duration::minutes(61)).unwrap());
        assert_eq!(AskLimits::default().auto_turns_per_hour, 60);
        let _ = std::fs::remove_dir_all(dir);
    }
}
