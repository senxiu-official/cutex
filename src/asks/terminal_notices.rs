//! Durable closure facts share the existing result outbox and controller routing.
use super::{model::*, store::AskState};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const PREFIX: &str = "terminal@";
pub const HISTORY_LIMIT: usize = 64;

fn retain_notice(state: &mut AskState, ask_id: &str, notice: Notice) {
    let evicted = if let Some(ask) = state.asks.get_mut(ask_id) {
        if !ask
            .terminal_notices
            .iter()
            .any(|n| n.ref_id == notice.ref_id)
        {
            ask.terminal_notices.push(notice);
        }
        let remove = ask.terminal_notices.len().saturating_sub(HISTORY_LIMIT);
        ask.terminal_notices.drain(..remove).collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    // Older releases stored only a reference. Freeze those payloads before
    // trimming too, including already claimed items whose receipts are fixed.
    for notice in evicted {
        for delivery in state.deliveries.values_mut().filter(|d| {
            d.ask_id == ask_id && d.notice_ref.as_deref().unwrap_or(&d.ref_id) == notice.ref_id
        }) {
            delivery
                .terminal_notice
                .get_or_insert_with(|| notice.clone());
        }
    }
}

pub(super) fn attach(state: &mut AskState, id: &str, notice: Notice, now: DateTime<Utc>) {
    // The authenticated control response already told this recipient what it
    // did. Keep the fact readable, without creating a turn to report it back.
    let self_caused = matches!(&notice.actor, Actor::Agent { session_id }
        if state.deliveries.get(id).and_then(|d|super::delivery::recipient(state,d)).as_ref()==Some(session_id));
    if let Some(delivery) = state.deliveries.get_mut(id) {
        delivery.notice_ref = Some(notice.ref_id.clone());
        delivery.terminal_notice = Some(notice);
        if self_caused
            && delivery.state == DeliveryState::Pending
            && delivery.unsubmitted
            && delivery.event_id.is_none()
        {
            delivery.state = DeliveryState::Acknowledged;
            delivery.settled_by = Some("selfCaused".into());
            delivery.updated_at = super::now_rfc3339(now);
        }
    }
}

pub(super) fn needs_retirement(state: &AskState, delivery: &DeliveryRecord) -> bool {
    is_critical(delivery)
        && delivery.state == DeliveryState::Pending
        && delivery.unsubmitted
        && delivery.event_id.is_none()
        && delivery.owner_input_id.is_none()
        && state
            .asks
            .get(&delivery.ask_id)
            .is_some_and(|a| !a.is_open())
}

pub(super) fn retire_unreachable(
    state: &mut AskState,
    ask_ids: &[&str],
    gone: impl Fn(&str) -> bool,
    now: DateTime<Utc>,
) {
    let ids: Vec<_> = state
        .deliveries
        .values()
        .filter(|d| ask_ids.contains(&d.ask_id.as_str()) && needs_retirement(state, d))
        // Unknown handover/authority is a hold, never proof of retirement.
        .filter(|d| super::delivery::recipient(state, d).is_some_and(|r| gone(&r)))
        .map(|d| d.delivery_id.clone())
        .collect();
    for id in ids {
        let delivery = state.deliveries.get_mut(&id).unwrap();
        delivery.state = DeliveryState::Acknowledged;
        delivery.settled_by = Some("recipientRetired".into());
        delivery.updated_at = super::now_rfc3339(now);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Actor {
    Owner,
    Agent {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    System {
        component: String,
    },
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cause {
    pub actor: Actor,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notice {
    pub ref_id: String,
    pub at: String,
    pub revision: u64,
    pub attempt: Option<u32>,
    pub execution: ExecutionState,
    pub closed: Option<CloseOutcome>,
    pub reason: String,
    pub actor: Actor,
}

pub fn set_cause(state: &mut AskState, id: &str, actor: Actor, reason: impl Into<String>) {
    if let Some(ask) = state.asks.get_mut(id) {
        ask.terminal_cause = Some(Cause {
            actor,
            reason: super::truncate_chars(&reason.into(), 1200),
        });
    }
}

pub fn control_cause(state: &mut AskState, id: &str, requester: Option<&str>, outcome: &str) {
    let actor = if let Some(session) = requester {
        Actor::Agent {
            session_id: session.into(),
        }
    } else if outcome == "stop" {
        let host = super::routing_policy::integration::local_host_id();
        match state
            .lineage
            .stop_fences
            .values()
            .filter(|f| f.root.id() == id && f.root.host_id() == host)
            .max_by_key(|f| f.created_at)
            .map(|f| f.requested_by.as_str())
        {
            Some("owner") | None => Actor::Owner,
            Some(session) => Actor::Agent {
                session_id: session.into(),
            },
        }
    } else {
        Actor::Owner
    };
    set_cause(state, id, actor, outcome);
}

/// Called once by update_ask on an actual open -> closed transition. This is
/// in the SAME candidate transaction as the outcome, not a later polling repair.
pub fn capture_closed(state: &mut AskState, ask: &AskRecord, now: DateTime<Utc>) {
    let ref_id = format!("{PREFIX}{}", ask.revision);
    if ask.terminal_notices.iter().any(|n| n.ref_id == ref_id) {
        return;
    }
    let reason = ask
        .admission_failure
        .clone()
        .or_else(|| ask.current_attempt().and_then(|r| r.last_error.clone()))
        .or_else(|| ask.remote.as_ref().and_then(|r| r.sync.last_error.clone()))
        .unwrap_or_else(|| {
            (if ask.closed == Some(CloseOutcome::Completed) {
                serde_json::to_value(ask.execution)
            } else {
                serde_json::to_value(ask.closed)
            })
            .unwrap()
            .as_str()
            .unwrap_or("closed")
            .into()
        });
    let routing_denial = ask
        .routing_approval_id
        .as_ref()
        .and_then(|id| state.routing.approvals.get(id))
        .filter(|a| {
            ask.execution == ExecutionState::Rejected
                && a.state == super::routing_policy::ApprovalState::Denied
        })
        .map(|a| {
            let actor = match a.decided_by.as_ref().filter(|by| !by.redacted) {
                Some(by) if by.kind == super::routing_policy::ActorKind::Owner => Actor::Owner,
                Some(by) if by.kind == super::routing_policy::ActorKind::Agent => by
                    .session_id
                    .clone()
                    .map(|session_id| Actor::Agent { session_id })
                    .unwrap_or(Actor::Unknown),
                _ => Actor::Unknown,
            };
            Cause {
                actor,
                reason: a
                    .decision_note
                    .clone()
                    .unwrap_or_else(|| "Routing approval denied".into()),
            }
        });
    let cause = ask
        .terminal_cause
        .clone()
        .or(routing_denial)
        .unwrap_or(Cause {
            actor: Actor::System {
                component: "cutex".into(),
            },
            reason,
        });
    let notice = Notice {
        ref_id: ref_id.clone(),
        at: super::now_rfc3339(now),
        revision: ask.revision,
        attempt: ask.current_attempt().map(|r| r.n),
        execution: ask.execution,
        closed: ask.closed,
        reason: cause.reason,
        actor: cause.actor,
    };
    retain_notice(state, &ask.ask_id, notice);
    // A normal answer already has a durable result. Administrative closures
    // and pre-dispatch failures have no answer and need their own state item.
    if ask.closed != Some(CloseOutcome::Completed)
        || ask.execution != ExecutionState::Answered
        || ask.answer_id.is_none()
    {
        super::delivery::record_terminal_notice(state, ask, ref_id, now);
    }
}

pub fn is_notice(delivery: &DeliveryRecord) -> bool {
    delivery.kind == DeliveryKind::State
        && (delivery.ref_id.starts_with(PREFIX)
            || delivery
                .notice_ref
                .as_deref()
                .is_some_and(|r| r.starts_with(PREFIX)))
}

pub fn is_critical(delivery: &DeliveryRecord) -> bool {
    delivery.notice_ref.is_some()
        || is_notice(delivery)
        || (delivery.kind == DeliveryKind::State
            && delivery.ref_id.starts_with(super::handler_failure::PREFIX))
}

pub fn capture_delivery(
    state: &mut AskState,
    ask: &AskRecord,
    id: &str,
    reason: &str,
    now: DateTime<Utc>,
) {
    let Some(reference) = state.deliveries.get(id).map(|d| d.ref_id.clone()) else {
        return;
    };
    let cause = ask.terminal_cause.clone().unwrap_or(Cause {
        actor: if reason == super::render::OWNER_STOPPED_STATE {
            Actor::Unknown
        } else {
            Actor::System {
                component: "cutex".into(),
            }
        },
        reason: reason.into(),
    });
    let notice = Notice {
        ref_id: reference.clone(),
        at: super::now_rfc3339(now),
        revision: ask.revision,
        attempt: ask.current_attempt().map(|a| a.n),
        execution: ask.execution,
        closed: ask.closed,
        actor: cause.actor,
        reason: cause.reason,
    };
    retain_notice(state, &ask.ask_id, notice.clone());
    attach(state, id, notice, now);
}

fn failure_notice(ask: &AskRecord, attempt: &AttemptRecord) -> Option<Notice> {
    let failure = attempt.handler_failure.as_ref()?;
    Some(Notice {
        ref_id: format!("{}{n}", super::handler_failure::PREFIX, n = attempt.n),
        at: failure.failed_at.clone(),
        revision: ask.revision,
        attempt: Some(attempt.n),
        execution: if attempt.state == AttemptState::Interrupted {
            ExecutionState::Interrupted
        } else {
            ExecutionState::Failed
        },
        closed: None,
        reason: failure.message.clone(),
        actor: Actor::System {
            component: "handlerRuntime".into(),
        },
    })
}

pub fn capture_failure(
    state: &mut AskState,
    ask: &AskRecord,
    n: u32,
    id: &str,
    now: DateTime<Utc>,
) {
    let Some(mut notice) = ask
        .attempts
        .iter()
        .find(|a| a.n == n)
        .and_then(|a| failure_notice(ask, a))
    else {
        return;
    };
    notice.execution = ask.execution;
    notice.closed = ask.closed;
    retain_notice(state, &ask.ask_id, notice.clone());
    attach(state, id, notice, now);
}

pub fn for_delivery(ask: &AskRecord, delivery: &DeliveryRecord) -> Option<Notice> {
    delivery
        .terminal_notice
        .clone()
        .or_else(|| {
            ask.terminal_notices
                .iter()
                .find(|n| n.ref_id == delivery.notice_ref.as_deref().unwrap_or(&delivery.ref_id))
                .cloned()
        })
        .or_else(|| {
            let n = delivery
                .ref_id
                .strip_prefix(super::handler_failure::PREFIX)?
                .parse::<u32>()
                .ok()?;
            failure_notice(ask, ask.attempts.iter().find(|a| a.n == n)?)
        })
}

pub fn readable(ask: &AskRecord) -> bool {
    ask.closed.is_some()
        || ask.execution.is_terminal()
        || ask
            .current_attempt()
            .is_some_and(|a| a.handler_failure.is_some())
}

pub fn read_state(ask: &AskRecord) -> Value {
    let notice = if ask.closed.is_some() {
        ask.terminal_notices
            .last()
            .filter(|n| n.closed == ask.closed)
            .cloned()
    } else {
        ask.current_attempt()
            .and_then(|a| {
                ask.terminal_notices
                    .iter()
                    .find(|n| n.ref_id == format!("{}{i}", super::handler_failure::PREFIX, i = a.n))
                    .cloned()
                    .or_else(|| failure_notice(ask, a))
            })
            .or_else(|| {
                ask.terminal_notices
                    .last()
                    .filter(|n| {
                        n.execution == ask.execution
                            && n.attempt == ask.current_attempt().map(|a| a.n)
                    })
                    .cloned()
            })
    };
    // Do not invent a historical owner/agent: old records did not store one.
    let notice = notice.unwrap_or_else(|| Notice {
        ref_id: format!("legacy@{}", ask.revision),
        at: ask
            .closed_at
            .clone()
            .unwrap_or_else(|| ask.updated_at.clone()),
        revision: ask.revision,
        attempt: ask.current_attempt().map(|r| r.n),
        execution: ask.execution,
        closed: ask.closed,
        actor: Actor::Unknown,
        reason: ask.admission_failure.clone().unwrap_or_else(|| {
            "Historical terminal state; its initiating actor/reason was not recorded".into()
        }),
    });
    json!({"ok":true,"ask_id":ask.ask_id,"kind":"state","execution":ask.execution,
        "closed":ask.closed,"terminal":ask.closed.is_some(),"resuming":ask.resume_pending(),"reason":notice.reason,
        "actor":notice.actor,"notice":notice,
        "handlerFailure":ask.current_attempt().and_then(|a|a.handler_failure.as_ref())})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::{
        delivery,
        store::{
            tests::{new_ask, temp_store, REQUESTER},
            AskStore,
        },
        tools,
    };

    #[test]
    fn requester_stop_and_abandon_are_readable_without_a_self_wake() {
        use crate::asks::scheduler::{
            self,
            tests::{world, FakeRuntime},
        };
        use crate::asks::store::tests::TARGET;
        for outcome in ["stop", "abandoned"] {
            let (world, wd) = world("self-stop-notice");
            let (store, dir) = temp_store("self-stop-notice");
            let now = Utc::now();
            let id = store
                .with_state(|s| {
                    s.create_ask(new_ask("self-stop", "work"), now)
                        .unwrap()
                        .0
                        .ask_id
                })
                .unwrap();
            let stopped = tools::tests::call(
                &world,
                &store,
                REQUESTER,
                if outcome == "stop" {
                    tools::DELEGATION_STOP
                } else {
                    tools::DELEGATION_CLOSE
                },
                if outcome == "stop" {
                    json!({"delegation":id})
                } else {
                    json!({"delegation":id,"outcome":outcome})
                },
                "self-stop",
            );
            assert_eq!(stopped["ok"], true, "{stopped}");
            let runtime = FakeRuntime::idle(&[REQUESTER, TARGET]);
            let report = scheduler::tick(&world, &runtime, &store, now).unwrap();
            assert!(report.events.is_empty());
            assert!(runtime.submitted.borrow().is_empty());
            let state = store.snapshot().unwrap();
            let item = state.deliveries.values().find(|d| is_notice(d)).unwrap();
            assert_eq!(item.state, DeliveryState::Acknowledged);
            assert_eq!(item.settled_by.as_deref(), Some("selfCaused"));
            assert_eq!(
                read_state(&state.asks[&id])["actor"]["sessionId"],
                REQUESTER
            );
            assert!(state.answers.is_empty());
            assert_eq!(
                tools::tests::call(
                    &world,
                    &store,
                    REQUESTER,
                    tools::AGENT_LIST,
                    json!({}),
                    "self-list"
                )["unclaimed_results"],
                0
            );
            assert_eq!(
                tools::tests::call(
                    &world,
                    &store,
                    REQUESTER,
                    tools::DELEGATION_READ,
                    json!({"delegation":id}),
                    "self-read"
                )["kind"],
                "state"
            );
            std::fs::remove_dir_all(dir).unwrap();
            if wd.exists() {
                std::fs::remove_dir_all(wd).unwrap();
            }
        }
    }

    #[test]
    fn another_agent_control_cause_still_notifies_the_recipient() {
        let (store, dir) = temp_store("other-control-notice");
        let now = Utc::now();
        store
            .with_state(|s| {
                let id = s
                    .create_ask(new_ask("other-stop", "work"), now)
                    .unwrap()
                    .0
                    .ask_id;
                set_cause(
                    s,
                    &id,
                    Actor::Agent {
                        session_id: "cutex.director-other".into(),
                    },
                    "stop",
                );
                s.update_ask(&id, now, |a| {
                    a.execution = ExecutionState::Cancelled;
                    a.closed = Some(CloseOutcome::Stopped);
                });
                let delivery = s.deliveries.values().find(|d| is_notice(d)).unwrap();
                assert_eq!(delivery.state, DeliveryState::Pending);
                assert!(delivery::actionable(s, delivery));
            })
            .unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bounded_notice_history_keeps_old_unclaimed_payload_after_restart() {
        let (store, dir) = temp_store("bounded-notice-history");
        let now = Utc::now();
        let id = store
            .with_state(|s| {
                let id = s
                    .create_ask(new_ask("bounded-history", "work"), now)
                    .unwrap()
                    .0
                    .ask_id;
                for n in 0..HISTORY_LIMIT + 3 {
                    s.update_ask(&id, now, |a| {
                        a.closed = None;
                        a.execution = ExecutionState::Queued;
                    });
                    set_cause(
                        s,
                        &id,
                        Actor::System {
                            component: "history-test".into(),
                        },
                        format!("closed-{n}"),
                    );
                    s.update_ask(&id, now, |a| {
                        a.execution = ExecutionState::Failed;
                        a.closed = Some(CloseOutcome::Completed);
                    });
                    if n == 0 {
                        // Before this follow-up a delivery only stored its ref.
                        for d in s.deliveries.values_mut() {
                            d.terminal_notice = None;
                        }
                    }
                }
                id
            })
            .unwrap();
        let reopened = AskStore::at(dir.clone());
        let state = reopened.snapshot().unwrap();
        let ask = &state.asks[&id];
        assert_eq!(ask.terminal_notices.len(), HISTORY_LIMIT);
        let oldest = state
            .deliveries
            .values()
            .find(|d| {
                d.terminal_notice
                    .as_ref()
                    .is_some_and(|n| n.reason == "closed-0")
            })
            .unwrap();
        assert!(!ask
            .terminal_notices
            .iter()
            .any(|n| Some(&n.ref_id) == oldest.notice_ref.as_ref()));
        assert_eq!(for_delivery(ask, oldest).unwrap().reason, "closed-0");
        assert_eq!(oldest.state, DeliveryState::Pending);
        assert_eq!(
            read_state(ask)["reason"],
            format!("closed-{}", HISTORY_LIMIT + 2)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn archived_or_forgotten_recipient_settles_unsubmitted_closed_notices_only() {
        use crate::asks::scheduler::{
            self,
            tests::{world, FakeRuntime},
        };
        for forgotten in [false, true] {
            let (mut world, wd) = world("retired-notice");
            let (store, dir) = temp_store("retired-notice");
            let now = Utc::now();
            let id = store
                .with_state(|s| {
                    s.create_ask(new_ask("retired-notice", "work"), now)
                        .unwrap()
                        .0
                        .ask_id
                })
                .unwrap();
            tools::close_ask(&store, None, &id, "abandoned", now).unwrap();
            let runtime = FakeRuntime::idle(&[]);
            scheduler::tick(&world, &runtime, &store, now).unwrap();
            assert!(
                store
                    .snapshot()
                    .unwrap()
                    .deliveries
                    .values()
                    .all(|d| d.state == DeliveryState::Pending),
                "offline is not retired"
            );
            if forgotten {
                world.directory.agents.remove(REQUESTER);
            } else {
                world.directory.agents.get_mut(REQUESTER).unwrap().archived = true;
            }
            scheduler::tick(&world, &runtime, &store, now).unwrap();
            assert!(store
                .snapshot()
                .unwrap()
                .deliveries
                .values()
                .all(|d| d.settled_by.as_deref() == Some("recipientRetired")));
            assert!(runtime.submitted.borrow().is_empty());
            std::fs::remove_dir_all(dir).unwrap();
            if wd.exists() {
                std::fs::remove_dir_all(wd).unwrap();
            }
        }
    }

    #[test]
    fn retirement_updates_only_selected_delegations() {
        let (store, dir) = temp_store("retire-selected-only");
        let now = Utc::now();
        let ids: Vec<_> = (0..2).map(|n| {
            let id = store.with_state(|s|s.create_ask(new_ask(&format!("selected-{n}"),"work"),now).unwrap().0.ask_id).unwrap();
            tools::close_ask(&store,None,&id,"abandoned",now).unwrap();
            id
        }).collect();
        store.with_records(&[&ids[0]],|s|retire_unreachable(s,&[&ids[0]],|_|true,now)).unwrap();
        let state = store.snapshot().unwrap();
        assert!(state.deliveries.values().any(|d|d.ask_id==ids[0]));
        assert!(state.deliveries.values().any(|d|d.ask_id==ids[1]));
        for d in state.deliveries.values() {
            if d.ask_id==ids[0] {
                assert_eq!(d.settled_by.as_deref(),Some("recipientRetired"));
            } else {
                assert_eq!(d.state,DeliveryState::Pending);
                assert!(d.settled_by.is_none());
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn retirement_preserves_uncertain_and_owner_reserved_deliveries() {
        let (store, dir) = temp_store("retire-proven-only");
        let now = Utc::now();
        for (n, reserved) in [(0, false), (1, true)] {
            let id = store
                .with_state(|s| {
                    s.create_ask(new_ask(&format!("retire-{n}"), "work"), now)
                        .unwrap()
                        .0
                        .ask_id
                })
                .unwrap();
            tools::close_ask(&store, None, &id, "abandoned", now).unwrap();
            store
                .with_state(|s| {
                    let d = s.deliveries.values_mut().find(|d| d.ask_id == id).unwrap();
                    if reserved {
                        d.owner_input_id = Some("owner-reservation".into());
                    } else {
                        d.unsubmitted = false;
                    }
                    retire_unreachable(s, &[&id], |_| true, now);
                    assert_eq!(
                        s.deliveries
                            .values()
                            .find(|d| d.ask_id == id)
                            .unwrap()
                            .state,
                        DeliveryState::Pending
                    );
                })
                .unwrap();
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn terminal_notice_survives_restart_and_tracks_current_controller_until_claimed() {
        let (store, dir) = temp_store("terminal-controller");
        let now = Utc::now();
        let id = store
            .with_state(|s| {
                s.create_ask(new_ask("terminal-controller", "work"), now)
                    .unwrap()
                    .0
                    .ask_id
            })
            .unwrap();
        tools::close_ask(&store, None, &id, "abandoned", now).unwrap();
        let restored = AskStore::at(dir.clone());
        restored
            .with_state(|s| {
                let ask = &s.asks[&id];
                assert_eq!(read_state(ask)["actor"]["kind"], "owner");
                assert_eq!(ask.terminal_notices.len(), 1);
                assert!(s.answers.is_empty());
                let item = s.deliveries.values().find(|d| is_notice(d)).unwrap();
                assert!(
                    delivery::actionable(s, item),
                    "closed abandonment must not hide its own final notice"
                );
                crate::asks::model::route::pin(
                    &id,
                    crate::asks::model::route::Pin {
                        controller: Ok(Some("cutex.successor".into())),
                        history: vec![REQUESTER.into()],
                        questions: None,
                    },
                );
                assert_eq!(
                    delivery::recipient(s, item).as_deref(),
                    Some("cutex.successor")
                );
                assert!(
                    delivery::claim_for_wait(s, REQUESTER, &[id.clone()], "former", now).is_empty()
                );
                let first =
                    delivery::claim_for_wait(s, "cutex.successor", &[id.clone()], "read", now);
                assert_eq!(first.len(), 1);
                let repeated =
                    delivery::claim_for_wait(s, "cutex.successor", &[id.clone()], "read", now);
                assert_eq!(repeated.len(), 1);
                assert_eq!(first[0].delivery_id, repeated[0].delivery_id);
                assert!(delivery::claim_for_wait(
                    s,
                    "cutex.successor",
                    &[id.clone()],
                    "later",
                    now
                )
                .is_empty());
            })
            .unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn legacy_terminal_read_reports_unknown_actor_without_a_synthetic_answer() {
        let (store, dir) = temp_store("terminal-legacy");
        let now = Utc::now();
        store
            .with_state(|s| {
                let id = s
                    .create_ask(new_ask("terminal-legacy", "work"), now)
                    .unwrap()
                    .0
                    .ask_id;
                // Historical input, not a new transition: no fabricated audit.
                let ask = s.asks.get_mut(&id).unwrap();
                ask.closed = Some(CloseOutcome::Stopped);
                ask.execution = ExecutionState::Interrupted;
                let value = read_state(ask);
                assert_eq!(value["actor"]["kind"], "unknown");
                assert_eq!(value["closed"], "stopped");
                assert!(s.answers.is_empty());
            })
            .unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn completed_business_closure_does_not_render_a_failure_as_success() {
        let (store, dir) = temp_store("terminal-failed-label");
        let now = Utc::now();
        let id = store
            .with_state(|s| {
                let id = s
                    .create_ask(new_ask("terminal-failed-label", "work"), now)
                    .unwrap()
                    .0
                    .ask_id;
                s.update_ask(&id, now, |a| {
                    a.execution = ExecutionState::Failed;
                    a.closed = Some(CloseOutcome::Completed);
                });
                id
            })
            .unwrap();
        let state = store.snapshot().unwrap();
        let delivery = state.deliveries.values().find(|d| d.ask_id == id).unwrap();
        let item = crate::asks::returns::return_item(&store, &state, delivery).unwrap();
        let value = crate::asks::returns::item_json(&item);
        assert_eq!(value["kind"], "state");
        assert_eq!(value["state"], "failed");
        assert_eq!(value["notice"]["reason"], "failed");
        assert!(state.answers.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
