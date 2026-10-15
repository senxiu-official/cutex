//! Revisions bound to a delegation (cutex-desktop
//! `docs/research/delegation-await.md` §3): `agent_tell {delegation,
//! supersedes?}` records `v<k>` (v1 is the original brief) and tracks how
//! far the note got: sent → delivered (native accepted it) → inContext (A4,
//! written into a turn) → acknowledged (`delegation_progress {ack}`).
//! Levels only rise.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use super::model::*;
use super::scheduler::SchedulerWorld;
use super::store::{AskError, AskState, AskStore};
use super::{now_rfc3339, truncate_chars};

/// The id of the original brief.
pub const BRIEF_REVISION: &str = "v1";

/// The id the next revision of `ask` gets.
pub fn next_id(ask: &AskRecord) -> String {
    format!("v{}", ask.revisions.len() + 2)
}

/// Check a revision of `ask` by its requester (before sending it).
pub fn check(ask: &AskRecord, requester: &str, supersedes: Option<&str>) -> Result<String, AskError> {
    if ask.remote.is_some() { return Err(AskError::new("remote_unsupported","remote brief revisions are unavailable in P0")); }
    if ask.requester.session() != Some(requester) {
        return Err(AskError::new("ask_not_found", "no such delegation of yours"));
    }
    if !ask.is_open() {
        return Err(AskError::new("ask_closed", "it is already closed; send a follow-up with agent_delegate instead").with(json!({"closed": ask.closed})));
    }
    if ask.revisions.len() >= MAX_REVISIONS {
        return Err(AskError::new("too_many_revisions", format!("a delegation takes at most {MAX_REVISIONS} revisions; send a follow-up delegation instead")));
    }
    if let Some(previous) = supersedes {
        if previous != BRIEF_REVISION && !ask.revisions.iter().any(|revision| revision.id == previous) {
            return Err(AskError::new("revision_not_found", format!("supersedes: no revision {} on this delegation (v1 is the brief)", truncate_chars(previous, 40))));
        }
    }
    Ok(next_id(ask))
}

/// The header line put before the revision text.
pub fn header(ask_id: &str, id: &str, supersedes: Option<&str>, requester: &str) -> String {
    let ask_id = crate::short_ids::display(ask_id);
    match supersedes {
        Some(previous) => format!("[delegation {ask_id} · revision {id} (supersedes {previous})] from {requester}"),
        None => format!("[delegation {ask_id} · revision {id}] from {requester}"),
    }
}

/// The confirmation hint at the end of the revision text (only for
/// handlers whose facade has the `ack` parameter).
pub fn footer(ask_id: &str, id: &str, facade: u32) -> String {
    let ask_id = crate::short_ids::display(ask_id);
    if facade >= super::tools::FACADE_DELEGATIONS {
        format!("(Confirm you applied it: delegation_progress(delegation=\"{ask_id}\", ack=\"{id}\", text=\"…\").)")
    } else {
        "(This revises the delegation you are handling.)".into()
    }
}

/// Record a sent revision.
pub fn record(
    state: &mut AskState,
    ask_id: &str,
    id: &str,
    text: &str,
    supersedes: Option<&str>,
    message_id: Option<String>,
    wake: bool,
    now: DateTime<Utc>,
) -> Option<RevisionRecord> {
    let record = RevisionRecord {
        id: id.to_string(),
        text: truncate_chars(text, 16 * 1024),
        supersedes: supersedes.map(str::to_string),
        message_id,
        wake,
        sent_at: now_rfc3339(now),
        level: RevisionLevel::Sent,
        delivered_at: None,
        in_context_at: None,
        turn_id: None,
        acknowledged_at: None,
        ack_text: None,
    };
    state.update_ask(ask_id, now, |ask| {
        if !ask.revisions.iter().any(|revision| revision.id == record.id) {
            ask.revisions.push(record.clone());
        }
    })?;
    Some(record)
}

/// Raise one revision's level (never lowers it).
pub fn raise(revision: &mut RevisionRecord, level: RevisionLevel, turn_id: Option<String>, now: DateTime<Utc>) -> bool {
    if level <= revision.level {
        return false;
    }
    let at = now_rfc3339(now);
    if level >= RevisionLevel::Delivered && revision.delivered_at.is_none() {
        revision.delivered_at = Some(at.clone());
    }
    if level >= RevisionLevel::InContext && revision.in_context_at.is_none() {
        revision.in_context_at = Some(at.clone());
        if revision.turn_id.is_none() {
            revision.turn_id = turn_id;
        }
    }
    if level == RevisionLevel::Acknowledged && revision.acknowledged_at.is_none() {
        revision.acknowledged_at = Some(at);
    }
    revision.level = level;
    true
}

/// The level a bus message reached (from an `AgentBusMessageSnapshot` as
/// JSON) and the turn it went into.
pub fn level_of_snapshot(snapshot: &Value) -> (RevisionLevel, Option<String>) {
    if let Some(turn) = snapshot.pointer("/a4Receipt/turnId").and_then(Value::as_str) {
        return (RevisionLevel::InContext, Some(turn.to_string()));
    }
    let delivery = snapshot.pointer("/externalInputLastObserved/deliveryState").and_then(Value::as_str);
    let turn = snapshot
        .pointer("/externalInputLastObserved/turnId")
        .or_else(|| snapshot.pointer("/externalInputLastObserved/receipt/turnId"))
        .and_then(Value::as_str)
        .map(str::to_string);
    if snapshot.get("state").and_then(Value::as_str) == Some("delivered")
        || matches!(delivery, Some("contextPersisted" | "context_persisted" | "consumed" | "inContext"))
    {
        return (RevisionLevel::InContext, turn);
    }
    if delivery.is_some() || snapshot.get("externalInputReceipt").is_some_and(|receipt| !receipt.is_null()) {
        return (RevisionLevel::Delivered, None);
    }
    (RevisionLevel::Sent, None)
}

/// Scheduler pass: raise revisions below `inContext` from the bus records.
pub fn reconcile(world: &dyn SchedulerWorld, store: &AskStore, now: DateTime<Utc>) -> anyhow::Result<()> {
    let Some(state) = store.peek()? else {
        return Ok(());
    };
    let pending: Vec<(String, String, String)> = state
        .asks
        .values()
        .filter(|ask| ask.is_open())
        .flat_map(|ask| {
            ask.revisions
                .iter()
                .filter(|revision| revision.level < RevisionLevel::InContext)
                .filter_map(|revision| Some((ask.ask_id.clone(), revision.id.clone(), revision.message_id.clone()?)))
        })
        .collect();
    if pending.is_empty() {
        return Ok(());
    }
    let mut raised = Vec::new();
    for (ask_id, id, message_id) in pending {
        if let Some((level, turn)) = world.message_level(&message_id) {
            raised.push((ask_id, id, level, turn));
        }
    }
    let current = |ask: &AskRecord, id: &str| ask.revisions.iter().find(|revision| revision.id == id).map(|revision| revision.level);
    if raised.iter().all(|(ask_id, id, level, _)| state.asks.get(ask_id).and_then(|ask| current(ask, id)).is_none_or(|known| *level <= known)) {
        return Ok(());
    }
    store.with_state(|state| {
        for (ask_id, id, level, turn) in &raised {
            state.update_ask(ask_id, now, |ask| {
                if let Some(revision) = ask.revisions.iter_mut().find(|revision| &revision.id == id) {
                    raise(revision, *level, turn.clone(), now);
                }
            });
        }
    })?;
    Ok(())
}

/// The handler acknowledges a revision (`delegation_progress {ack}`); any
/// turn of the handler may, no lease needed.
pub fn acknowledge(state: &mut AskState, handler: &str, ask_id: &str, id: &str, text: &str, now: DateTime<Utc>) -> Result<Value, AskError> {
    let ask = state
        .asks
        .get(ask_id)
        .filter(|ask| (ask.remote.is_none() && ask.target_cutex_session_id == handler))
        .cloned()
        .ok_or_else(|| AskError::new("ask_not_found", "no such delegation to you"))?;
    if !ask.is_open() {
        return Err(AskError::new("ask_closed", "it is already closed").with(json!({"closed": ask.closed})));
    }
    if !ask.revisions.iter().any(|revision| revision.id == id) {
        return Err(AskError::new("revision_not_found", format!("no revision {} on this delegation", truncate_chars(id, 40))));
    }
    // Kept whole (the tool refuses texts over `PROGRESS_MAX_CHARS`).
    let line = text.trim().to_string();
    state.update_ask(ask_id, now, |ask| {
        if let Some(revision) = ask.revisions.iter_mut().find(|revision| revision.id == id) {
            raise(revision, RevisionLevel::Acknowledged, None, now);
            revision.ack_text = Some(line.clone());
        }
    });
    state.notice(
        "cutex/ask/progress",
        AskState::ask_sessions(&ask),
        json!({"askId": ask_id, "text": line, "at": now_rfc3339(now), "ack": id}),
    );
    Ok(json!({"ok": true, "ask_id": ask_id, "ack": id, "detail": "Acknowledged; the requester sees that you applied this revision."}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::store::tests::{new_ask, REQUESTER, TARGET};

    #[test]
    fn levels_rise_and_acks_need_the_handler() {
        let now = Utc::now();
        let mut state = AskState::default();
        let ask_id = state.create_ask(new_ask("a", "brief"), now).unwrap().0.ask_id;
        let ask = state.asks[&ask_id].clone();
        assert_eq!(check(&ask, TARGET, None).unwrap_err().code, "ask_not_found");
        assert_eq!(check(&ask, REQUESTER, Some("v7")).unwrap_err().code, "revision_not_found");
        let id = check(&ask, REQUESTER, Some("v1")).unwrap();
        assert_eq!(id, "v2");
        record(&mut state, &ask_id, &id, "use the new API", Some("v1"), Some("msg-1".into()), false, now).unwrap();
        assert_eq!(check(&state.asks[&ask_id], REQUESTER, Some("v2")).unwrap(), "v3");
        assert!(state.notices.iter().any(|notice| notice.method == "cutex/ask/updated"));
        // Snapshot levels.
        assert_eq!(level_of_snapshot(&json!({"state": "pending"})).0, RevisionLevel::Sent);
        assert_eq!(level_of_snapshot(&json!({"state": "pending", "externalInputLastObserved": {"deliveryState": "pending"}})).0, RevisionLevel::Delivered);
        assert_eq!(
            level_of_snapshot(&json!({"state": "delivered", "a4Receipt": {"turnId": "turn-9"}})),
            (RevisionLevel::InContext, Some("turn-9".into()))
        );
        // Rise only.
        let mut revision = state.asks[&ask_id].revisions[0].clone();
        assert!(raise(&mut revision, RevisionLevel::InContext, Some("turn-9".into()), now));
        assert!(revision.delivered_at.is_some() && revision.in_context_at.is_some());
        assert!(!raise(&mut revision, RevisionLevel::Delivered, None, now));
        assert_eq!(revision.level, RevisionLevel::InContext);
        // Ack.
        assert_eq!(acknowledge(&mut state, REQUESTER, &ask_id, "v2", "ok", now).unwrap_err().code, "ask_not_found");
        assert_eq!(acknowledge(&mut state, TARGET, &ask_id, "v9", "ok", now).unwrap_err().code, "revision_not_found");
        acknowledge(&mut state, TARGET, &ask_id, "v2", "switched to the new API", now).unwrap();
        let revision = &state.asks[&ask_id].revisions[0];
        assert_eq!(revision.level, RevisionLevel::Acknowledged);
        assert_eq!(revision.ack_text.as_deref(), Some("switched to the new API"));
        assert!(state.notices.iter().any(|notice| notice.method == "cutex/ask/progress" && notice.params["ack"] == "v2"));
    }
}
