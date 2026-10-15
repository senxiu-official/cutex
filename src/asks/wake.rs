//! Auto-wake of offline targets (asks design §8.4, owner decision
//! 2026-10-07, §13 item 5).
//!
//! Default policy: an ask brings an offline target online only when it is
//! the requester's own child, or a same-project peer that went offline by
//! crash or idle shutdown; never an agent the owner stopped, paused or
//! archived; always within the per-domain runtime budget (agent-started
//! online runtimes). Owner asks wake any non-archived, non-paused target.
//! The profile gate and boundary are rechecked by the scheduler at
//! admission as for any ask.
//!
//! Explicit-launch agents without a live runtime need the owner's reviewed
//! launch (`ownerStartRequired`), with one exception (owner approval
//! 2026-10-14): a delegation from the target's creator or from the
//! Director of the target's project may start it again on the owner's
//! latest reviewed launch when that launch ran Ready and still matches the
//! agent's contract and configuration (`Provenance::reviewed_relaunch`,
//! `agent_management::reviewed_relaunch_receipt`). Every other check still
//! applies (archived, paused, owner-stopped, policy, budget). An unclean
//! stop (host reboot, process death) never marks a target owner-stopped;
//! an owner `cutex/runtime/offline` / `close` does and keeps it blocked.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde_json::json;

use super::children::Provenance;
use super::model::*;
use super::store::{AskState, AskStore};
use super::{now_rfc3339, parse_time};

/// A wake is not retried for the same target within this time.
pub const WAKE_RETRY_SECS: i64 = 120;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WakeDecision {
    Wake { domain: String },
    /// Start an explicit-launch agent again on the owner's latest reviewed
    /// launch, for `requester`'s delegation (see the module docs).
    RelaunchReviewed { domain: String, requester: String },
    Never(&'static str),
    OverBudget { domain: String },
}

/// The policy decision for waking `target` for `ask`.
pub fn decide(
    state: &AskState,
    provenance: &BTreeMap<String, Provenance>,
    ask: &AskRecord,
    archived: bool,
    online_budget: usize,
    online: &dyn Fn(&str) -> bool,
) -> WakeDecision {
    if matches!(ask.requester, super::model::Requester::Peer { .. }) || ask.remote.is_some() { return WakeDecision::Never("policy"); }
    let target = ask.target_cutex_session_id.as_str();
    if archived {
        return WakeDecision::Never("archived");
    }
    if state.pauses.contains_key(target) {
        return WakeDecision::Never("ownerPaused");
    }
    let target_provenance = provenance.get(target);
    let domain = target_provenance
        .map(|item| item.origin_domain.clone())
        .unwrap_or_else(|| format!("agent:{target}"));
    let mut reviewed_relaunch_by = None;
    if let Some(item) = target_provenance.filter(|item| item.owner_start_required) {
        // An explicit-launch agent with no live runtime: starting it needs
        // the owner's reviewed launch. Its creator or its project's Director
        // may reuse the owner's latest reviewed launch while it still
        // matches; nobody else, and never without one.
        let requester = ask.requester.session();
        let creator = requester.is_some_and(|requester| item.created_by.as_deref() == Some(requester));
        let director = requester.and_then(|requester| provenance.get(requester)).is_some_and(|requester| {
            requester.director && item.origin_domain.starts_with("project:") && requester.origin_domain == item.origin_domain
        });
        if !item.reviewed_relaunch || !(creator || director) {
            return WakeDecision::Never("ownerStartRequired");
        }
        reviewed_relaunch_by = requester.map(str::to_string);
    }
    if let Some(requester) = ask.requester.session() {
        if state.owner_offline.contains(target) {
            return WakeDecision::Never("ownerStopped");
        }
        let own_child = target_provenance.is_some_and(|item| item.created_by.as_deref() == Some(requester));
        let same_project = match (target_provenance, provenance.get(requester)) {
            (Some(target), Some(requester)) => {
                target.origin_domain.starts_with("project:") && target.origin_domain == requester.origin_domain
            }
            _ => false,
        };
        if !own_child && !same_project {
            return WakeDecision::Never("policy");
        }
    }
    let running = state
        .woken
        .iter()
        .filter(|(session, (woken_domain, _))| *woken_domain == domain && online(session))
        .count();
    if running >= online_budget {
        return WakeDecision::OverBudget { domain };
    }
    match reviewed_relaunch_by {
        Some(requester) => WakeDecision::RelaunchReviewed { domain, requester },
        None => WakeDecision::Wake { domain },
    }
}

/// Host event when a delegation starts an explicit-launch agent again on the
/// owner's earlier reviewed launch (traceability; the new launch receipt's
/// action id names the requester too).
pub const REVIEWED_RELAUNCH_EVENT: &str = "cutex/ask/reviewedRelaunch";

/// Record a reviewed relaunch for the owner (one notice per launch that
/// actually ran; `reviewed_action`: the owner's reviewed action it rests on).
pub fn record_reviewed_relaunch(state: &mut AskState, target: &str, requester: &str, ask_id: &str, reviewed_action: &str) {
    state.notice(
        REVIEWED_RELAUNCH_EVENT,
        vec![crate::management::v2::services::HOST_EVENT_SESSION_ID.to_string()],
        json!({"target": target, "requester": requester, "askId": ask_id, "basis": "ownerReviewedLaunch", "reviewedActionId": reviewed_action}),
    );
}

/// The owner's reviewed action a finished reviewed-relaunch start reports
/// (`startedBy.reviewedActionId`), or `None` when no relaunch ran: the
/// start found a live runtime, i.e. the owner's own start got there first.
/// Only then is the owner told about a relaunch.
pub fn relaunch_reviewed_action(result: &serde_json::Value) -> Option<&str> {
    result.pointer("/startedBy/reviewedActionId").and_then(serde_json::Value::as_str)
}

/// Best-effort store write of [`record_reviewed_relaunch`] (management
/// process, after the background relaunch).
pub fn record_reviewed_relaunch_default(target: &str, requester: &str, ask_id: &str, reviewed_action: &str) {
    let result = AskStore::open_default()
        .and_then(|store| store.with_state(|state| record_reviewed_relaunch(state, target, requester, ask_id, reviewed_action)));
    if let Err(error) = result {
        eprintln!("asks: could not record the reviewed relaunch of {target}: {error:#}");
    }
}

/// Whether a wake for `target` was tried recently.
pub fn recently_woken(state: &AskState, target: &str, now: DateTime<Utc>) -> bool {
    state
        .woken
        .get(target)
        .and_then(|(_, at)| parse_time(at))
        .is_some_and(|at| now - at < chrono::Duration::seconds(WAKE_RETRY_SECS))
}

/// Record a wake attempt (counts against the domain's runtime budget).
pub fn record_wake(state: &mut AskState, target: &str, domain: &str, now: DateTime<Utc>) {
    state.woken.insert(target.to_string(), (domain.to_string(), now_rfc3339(now)));
}

/// Over budget: hold the ask `blocked(budget)` and notify the owner once.
pub fn hold_over_budget(state: &mut AskState, ask_id: &str, domain: &str, now: DateTime<Utc>) {
    let changed = state.update_ask(ask_id, now, |ask| {
        let hold = Some(Hold::Blocked { by: BlockedReason::Budget });
        let changed = ask.hold != hold;
        ask.hold = hold;
        changed
    });
    if changed == Some(true) {
        if let Some(ask) = state.asks.get(ask_id).cloned() {
            state.notice(
                "cutex/ask/budgetExceeded",
                vec![crate::management::v2::services::HOST_EVENT_SESSION_ID.to_string()],
                json!({"budget": "agentStartedOnlinePerDomain", "key": domain, "askId": ask_id, "target": ask.target_cutex_session_id}),
            );
            super::delivery::record_state_delivery(state, &ask, "blockedBudget", now);
        }
    }
}

/// Host event when queued asks wait for an offline target only the owner can
/// bring online (r208).
pub const OWNER_ACTION_EVENT: &str = "cutex/ask/ownerActionRequired";

/// What a wake refusal means for the requester (tool results, texts).
pub fn never_detail(reason: &str) -> &'static str {
    match reason {
        "ownerStopped" => "the owner took it offline",
        "ownerPaused" => "the owner paused its delegations",
        "ownerStartRequired" => "it can only be started by the owner",
        "archived" => "it is archived",
        "policy" => "it is neither your child nor an agent of your project",
        "budget" => "the runtime budget of its domain is used up",
        _ => "the wake failed",
    }
}

/// `target` is offline and cutex can't bring it online (`reason`: a wake
/// policy refusal, or a failed wake only the owner can fix): every queued
/// ask to it gets an actionable `blockedOwnerAction` state item (its
/// requester is woken when idle and decides: wait, reassign or stop), and
/// the owner one `cutex/ask/ownerActionRequired` notice per newly affected
/// ask. Returns the newly affected asks.
pub fn record_owner_action(state: &mut AskState, target: &str, reason: &str, detail: Option<&str>, now: DateTime<Utc>) -> Vec<String> {
    let queued: Vec<AskRecord> = state
        .waiting_for_target(target)
        .into_iter()
        .filter(|ask| ask.execution == ExecutionState::Queued)
        .cloned()
        .collect();
    let known = state.owner_action.get(target).cloned().unwrap_or_default();
    let fresh: Vec<String> = queued
        .iter()
        .filter(|ask| !known.contains(&ask.ask_id))
        .map(|ask| ask.ask_id.clone())
        .collect();
    for ask in &queued {
        // One actionable item per (requester, target, reason) while the
        // target stays blocked; further asks of the same pair get a passive
        // item (delegation_wait / agent_list still show it) instead of
        // another turn each. A new reason, or an unblock
        // ([`clear_owner_action`]), may notify again.
        let Some(requester) = ask.requester.session() else {
            continue;
        };
        let key = owner_action_told_key(requester, target);
        let told = state.owner_action_told.get(&key).is_some_and(|told| told == reason);
        if super::delivery::record_owner_action_delivery_with(state, ask, told, now).is_some() && !told {
            state.owner_action_told.insert(key, reason.to_string());
        }
    }
    let still_queued: Vec<String> = queued.iter().map(|ask| ask.ask_id.clone()).collect();
    if still_queued.is_empty() {
        state.owner_action.remove(target);
        forget_owner_action_told(state, target);
    } else {
        state.owner_action.insert(target.to_string(), still_queued);
    }
    if !fresh.is_empty() {
        state.notice(
            OWNER_ACTION_EVENT,
            vec![crate::management::v2::services::HOST_EVENT_SESSION_ID.to_string()],
            json!({"target": target, "askIds": fresh, "reason": reason, "detail": detail.unwrap_or(never_detail(reason))}),
        );
    }
    fresh
}

/// The target is online again: forget who was told.
pub fn clear_owner_action(state: &mut AskState, target: &str) {
    state.owner_action.remove(target);
    forget_owner_action_told(state, target);
}

/// Key of [`AskState::owner_action_told`].
pub fn owner_action_told_key(requester: &str, target: &str) -> String {
    format!("{requester}\u{1f}{target}")
}

fn forget_owner_action_told(state: &mut AskState, target: &str) {
    let suffix = format!("\u{1f}{target}");
    state.owner_action_told.retain(|key, _| !key.ends_with(&suffix));
}

/// Best-effort store write for a failed background wake.
pub fn record_owner_action_default(target: &str, reason: &str, detail: &str) {
    let result = AskStore::open_default()
        .and_then(|store| store.with_state(|state| record_owner_action(state, target, reason, Some(detail), Utc::now())));
    if let Err(error) = result {
        eprintln!("asks: could not record that {target} needs the owner: {error:#}");
    }
}

/// A settled durable runtime binding whose process exists and did not start
/// after the binding (PID reuse): the runtime is live even when this process
/// holds no connection to it. Presence evidence and a reconnect trigger,
/// never launch authority.
pub fn runtime_binding_live(record: &crate::session::model::CutexSessionRecord) -> bool {
    let Some(binding) = record.app_server_runtime.as_ref() else {
        return false;
    };
    if record.app_server_launch_claim_id.is_some() || record.current_runtime_agent_id.is_none() {
        return false;
    }
    let Ok(bound_at) = chrono::DateTime::parse_from_rfc3339(&binding.started_at) else {
        return false;
    };
    crate::platform::process::process_started_at(binding.pid)
        .is_ok_and(|started| started <= bound_at.with_timezone(&Utc) + chrono::Duration::seconds(60))
}

/// Management v2 hook: the owner took a runtime offline (or online).
pub fn note_owner_runtime_action(state: &mut AskState, session: &str, offline: bool) {
    if offline {
        state.owner_offline.insert(session.to_string());
        state.woken.remove(session);
    } else {
        state.owner_offline.remove(session);
    }
}

pub fn note_owner_runtime_action_default(session: &str, offline: bool) {
    let Ok(store) = AskStore::open_default() else {
        return;
    };
    // Bringing an agent online never needs to create the store.
    if !offline && !matches!(store.peek(), Ok(Some(_))) {
        return;
    }
    if let Err(error) = store.with_state(|state| note_owner_runtime_action(state, session, offline)) {
        eprintln!("asks: could not record the owner runtime action for {session}: {error:#}");
    }
}

/// Every owner start path (HTTP `cutex/runtime/online`, an owner submit
/// that starts an offline agent, CLI `cutex human start` / `restart` /
/// `session online`): the owner wants it running again, so it is no longer
/// owner-stopped for auto-wake / reviewed relaunch. Called once the start
/// succeeded: clearing it first let an asks relaunch race the owner's own
/// start (seen with `cutex human start`: the relaunch won, the owner's start
/// failed and left a `prepared` receipt). Best effort.
pub fn note_owner_start(session: &str) {
    note_owner_runtime_action_default(session, false);
}

/// Why a startup prune dropped an owner-stop mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StaleOwnerMark {
    /// The agent no longer exists.
    Removed,
    /// Its runtime is online now.
    Online,
    /// The owner started it after the mark (an online intent exists, which
    /// a requested offline / close removes; for an explicit-launch agent its
    /// latest runtime receipt is an owner-started Ready launch).
    OwnerStarted,
}

/// Owner-stop marks that are no longer valid (left by releases whose HTTP
/// online did not clear them). `intents`: the runtime online intent store
/// (`session::runtime_intent`). A mark is kept unless one of
/// [`StaleOwnerMark`] holds. Pure; see [`prune_stale_owner_marks_at_startup`].
pub fn stale_owner_marks(
    state: &AskState,
    sessions: &crate::session::model::CutexSessionStore,
    intents: &crate::session::runtime_intent::IntentState,
    online: &dyn Fn(&crate::session::model::CutexSessionRecord) -> bool,
) -> Vec<(String, StaleOwnerMark)> {
    state
        .owner_offline
        .iter()
        .filter_map(|id| {
            let Some(record) = sessions.sessions.get(id) else {
                return Some((id.clone(), StaleOwnerMark::Removed));
            };
            if online(record) {
                return Some((id.clone(), StaleOwnerMark::Online));
            }
            let owner_started = intents.intents.contains_key(id)
                && (record.explicit_launch.is_none()
                    || crate::agent_management::latest_runtime_receipt(sessions, id).is_some_and(|receipt| {
                        receipt.stage == crate::agent_management::StockRuntimeStage::Ready
                            && receipt.error.is_none()
                            && crate::agent_management::owner_initiated_runtime_receipt(receipt)
                    }));
            owner_started.then(|| (id.clone(), StaleOwnerMark::OwnerStarted))
        })
        .collect()
}

/// Management start (after runtime reconciliation): drop stale owner-stop
/// marks and log each. Idempotent; an older release just sees fewer marks.
pub fn prune_stale_owner_marks_at_startup() {
    let result = (|| -> anyhow::Result<Vec<(String, StaleOwnerMark)>> {
        let store = AskStore::open_default()?;
        if !matches!(store.peek(), Ok(Some(ref state)) if !state.owner_offline.is_empty()) {
            return Ok(Vec::new());
        }
        let sessions = crate::session::store::load_cutex_session_store()?;
        let intents = crate::session::runtime_intent::load()?;
        store.with_state(|state| {
            let stale = stale_owner_marks(state, &sessions, &intents, &crate::session::runtime_intent::record_online);
            for (id, _) in &stale {
                state.owner_offline.remove(id);
            }
            stale
        })
    })();
    match result {
        Ok(stale) => {
            for (id, why) in stale {
                println!("asks: cleared the stale owner-stop mark of {id} ({why:?})");
            }
        }
        Err(error) => eprintln!("asks: owner-stop mark cleanup warning: {error:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::children::{provenance, AgentFacts};
    use crate::asks::store::tests::{new_ask, REQUESTER, TARGET};

    fn facts(id: &str, created_by: Option<&str>, project: Option<&str>) -> AgentFacts {
        AgentFacts {
            cutex_session_id: id.into(),
            created_by: created_by.map(str::to_string),
            project: project.map(str::to_string),
            ..AgentFacts::default()
        }
    }

    /// Acceptance §12.3 item 10 (part): auto-wake respects the policy, an
    /// owner-stopped target and the runtime budget.
    #[test]
    fn auto_wake_policy() {
        let mut state = AskState::default();
        let now = Utc::now();
        let (ask, _) = state.create_ask(new_ask("a", "x"), now).unwrap();
        let offline = |_: &str| false;
        let online = |_: &str| true;
        // Same-project peer offline by crash: wake.
        let peers = provenance(&[facts(REQUESTER, None, Some("alpha")), facts(TARGET, None, Some("alpha"))]);
        assert_eq!(decide(&state, &peers, &ask, false, 6, &offline), WakeDecision::Wake { domain: "project:alpha".into() });
        // Unrelated independent agents: never.
        let strangers = provenance(&[facts(REQUESTER, None, None), facts(TARGET, None, None)]);
        assert_eq!(decide(&state, &strangers, &ask, false, 6, &offline), WakeDecision::Never("policy"));
        // Own child (independent tree): wake.
        let child = provenance(&[facts(REQUESTER, None, None), facts(TARGET, Some(REQUESTER), None)]);
        assert!(matches!(decide(&state, &child, &ask, false, 6, &offline), WakeDecision::Wake { .. }));
        // Archived and owner-stopped: never.
        assert_eq!(decide(&state, &peers, &ask, true, 6, &offline), WakeDecision::Never("archived"));
        note_owner_runtime_action(&mut state, TARGET, true);
        assert_eq!(decide(&state, &peers, &ask, false, 6, &offline), WakeDecision::Never("ownerStopped"));
        note_owner_runtime_action(&mut state, TARGET, false);
        // Budget: one runtime already woken in the domain and still online.
        record_wake(&mut state, "other", "project:alpha", now);
        assert!(matches!(decide(&state, &peers, &ask, false, 1, &online), WakeDecision::OverBudget { .. }));
        assert!(matches!(decide(&state, &peers, &ask, false, 1, &offline), WakeDecision::Wake { .. }));
        hold_over_budget(&mut state, &ask.ask_id, "project:alpha", now);
        assert_eq!(state.asks[&ask.ask_id].hold, Some(Hold::Blocked { by: BlockedReason::Budget }));
        assert!(recently_woken(&state, "other", now));
        assert!(!recently_woken(&state, "other", now + chrono::Duration::minutes(3)));
    }

    /// Only a start that actually relaunched reports the owner's reviewed
    /// action; the owner's own start getting there first does not.
    #[test]
    fn relaunch_notice_only_for_a_launch_that_ran() {
        let ran = json!({"status": "online", "startedBy": {"requester": REQUESTER, "reviewedActionId": "api-runtime-1", "basis": "ownerReviewedLaunch"}});
        assert_eq!(relaunch_reviewed_action(&ran), Some("api-runtime-1"));
        let reconnected = json!({"status": "online", "startedBy": {"requester": REQUESTER, "reviewedActionId": null, "basis": "ownerReviewedLaunch"}});
        assert_eq!(relaunch_reviewed_action(&reconnected), None);
        let mut state = AskState::default();
        record_reviewed_relaunch(&mut state, TARGET, REQUESTER, "ask-1", "api-runtime-1");
        assert_eq!(state.notices[0].params["reviewedActionId"], "api-runtime-1");
    }

    fn explicit(id: &str, created_by: Option<&str>, project: Option<&str>, reviewed: bool) -> AgentFacts {
        AgentFacts { explicit_launch: true, reviewed_relaunch: reviewed, ..facts(id, created_by, project) }
    }

    fn director(id: &str, project: &str) -> AgentFacts {
        AgentFacts { director: true, ..facts(id, None, Some(project)) }
    }

    /// Owner approval 2026-10-14: an offline explicit-launch worker is
    /// started again on the owner's reviewed launch only for its creator or
    /// its project's Director, and only while that review still matches;
    /// archived / paused / owner-stopped / budget still apply.
    #[test]
    fn reviewed_relaunch_policy() {
        let mut state = AskState::default();
        let now = Utc::now();
        let (ask, _) = state.create_ask(new_ask("a", "x"), now).unwrap();
        let offline = |_: &str| false;
        let online = |_: &str| true;
        let relaunch = WakeDecision::RelaunchReviewed { domain: "project:alpha".into(), requester: REQUESTER.into() };
        // Eligible: the project's Director, reviewed launch still matches.
        let eligible = provenance(&[director(REQUESTER, "alpha"), explicit(TARGET, None, Some("alpha"), true)]);
        assert_eq!(decide(&state, &eligible, &ask, false, 6, &offline), relaunch);
        // Eligible: the worker's creator (an independent child tree).
        let child = provenance(&[facts(REQUESTER, None, None), explicit(TARGET, Some(REQUESTER), None, true)]);
        assert!(matches!(
            decide(&state, &child, &ask, false, 6, &offline),
            WakeDecision::RelaunchReviewed { ref requester, .. } if requester == REQUESTER
        ));
        // No matching reviewed launch (never launched, configuration or
        // contract changed, receipt pruned, newer failed launch): owner only.
        let unreviewed = provenance(&[director(REQUESTER, "alpha"), explicit(TARGET, None, Some("alpha"), false)]);
        assert_eq!(decide(&state, &unreviewed, &ask, false, 6, &offline), WakeDecision::Never("ownerStartRequired"));
        // A same-project peer that is neither Director nor creator.
        let peer = provenance(&[facts(REQUESTER, None, Some("alpha")), explicit(TARGET, None, Some("alpha"), true)]);
        assert_eq!(decide(&state, &peer, &ask, false, 6, &offline), WakeDecision::Never("ownerStartRequired"));
        // Another project's Director.
        let other = provenance(&[director(REQUESTER, "beta"), explicit(TARGET, None, Some("alpha"), true)]);
        assert_eq!(decide(&state, &other, &ask, false, 6, &offline), WakeDecision::Never("ownerStartRequired"));
        // A live binding is no owner start at all (reconnect path), and the
        // fact only counts with an offline explicit launch.
        let mut live = explicit(TARGET, None, Some("alpha"), true);
        live.binding_live = true;
        let live = provenance(&[director(REQUESTER, "alpha"), live]);
        assert_eq!(decide(&state, &live, &ask, false, 6, &offline), WakeDecision::Wake { domain: "project:alpha".into() });
        // Owner asks never relaunch an explicit-launch agent automatically.
        let mut owner_ask = ask.clone();
        owner_ask.requester = Requester::Owner;
        assert_eq!(decide(&state, &eligible, &owner_ask, false, 6, &offline), WakeDecision::Never("ownerStartRequired"));
        // Archived, paused, owner-stopped: blocked as before.
        assert_eq!(decide(&state, &eligible, &ask, true, 6, &offline), WakeDecision::Never("archived"));
        note_owner_runtime_action(&mut state, TARGET, true);
        assert_eq!(decide(&state, &eligible, &ask, false, 6, &offline), WakeDecision::Never("ownerStopped"));
        note_owner_runtime_action(&mut state, TARGET, false);
        assert_eq!(decide(&state, &eligible, &ask, false, 6, &offline), relaunch, "owner online again: eligible");
        state.pauses.insert(TARGET.into(), AdmissionPause {
            cutex_session_id: TARGET.into(), by: PausedBy::Owner,
            since: now_rfc3339(now), origin: Some("ownerPause".into()), stop_hold: false, runtime_generation: None,
        });
        assert_eq!(decide(&state, &eligible, &ask, false, 6, &offline), WakeDecision::Never("ownerPaused"));
        state.pauses.remove(TARGET);
        // Budget: counted like any agent-started runtime of the domain.
        record_wake(&mut state, "other", "project:alpha", now);
        assert_eq!(
            decide(&state, &eligible, &ask, false, 1, &online),
            WakeDecision::OverBudget { domain: "project:alpha".into() }
        );
        assert_eq!(decide(&state, &eligible, &ask, false, 1, &offline), relaunch);
    }
}
