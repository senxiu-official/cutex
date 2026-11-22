//! r290 P7: terminal handler errors are state notices, never successful results.
use super::{model::*, now_rfc3339, store::AskState, truncate_chars};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const PREFIX: &str = "handlerTurnFailed@";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    ProviderRefused,
    Quota,
    RateLimited,
    RuntimeError,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandlerFailure {
    #[serde(default)]
    pub auto_requeued: bool,
    #[serde(default)]
    pub resumes_at: Option<String>,
    #[serde(default)]
    pub resets_at: Option<String>,
    #[serde(default)]
    pub hold: Option<Hold>,
    #[serde(default)]
    pub gate_closed: bool,
    pub kind: FailureKind,
    pub message: String,
    pub code: Option<String>,
    pub turn_id: Option<String>,
    pub last_activity: Activity,
    pub delegation_open: bool,
    pub failed_at: String,
}

fn code(error: &Value) -> Option<&str> {
    let info = error.get("codexErrorInfo");
    let native = match info {
        Some(Value::String(code)) => Some(code.as_str()),
        Some(Value::Object(map)) => map.keys().next().map(String::as_str),
        _ => None,
    };
    native.filter(|code| *code != "other").or_else(|| {
        error
            .get("code")
            .or_else(|| error.get("type"))
            .and_then(Value::as_str)
            .filter(|code| !matches!(*code, "other" | "unknown"))
    })
}

pub fn diagnostic(
    error: &Value,
    turn_id: Option<&str>,
    activity: &Activity,
    now: DateTime<Utc>,
) -> HandlerFailure {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .unwrap_or("Handler turn failed");
    // Providers sometimes put their structured response inside the runtime message.
    let embedded = message
        .find('{')
        .and_then(|at| serde_json::from_str::<Value>(&message[at..]).ok());
    let provider = embedded.as_ref().map(|v| v.get("error").unwrap_or(v));
    let code = code(error).or_else(|| provider.and_then(code));
    let kind = match code {
        Some(
            "cyberPolicy"
            | "contentPolicyViolation"
            | "content_filter"
            | "content_policy_violation"
            | "policy_violation"
            | "refusal"
            | "safety",
        ) => FailureKind::ProviderRefused,
        Some(
            "usageLimitExceeded" | "sessionBudgetExceeded" | "insufficient_quota" | "billing_error",
        ) => FailureKind::Quota,
        Some("rateLimitExceeded" | "rate_limit" | "rate_limit_error") => FailureKind::RateLimited,
        Some(
            "internalServerError"
            | "serverOverloaded"
            | "flexUnavailable"
            | "modelUnavailable"
            | "model_not_found"
            | "overloaded_error"
            | "server_error"
            | "responseStreamDisconnected"
            | "responseStreamConnectionFailed"
            | "responseTooManyFailedAttempts"
            | "runtime_error"
            | "runtimeExited",
        ) => FailureKind::RuntimeError,
        Some(_) => FailureKind::Other,
        None => {
            let text = message.to_ascii_lowercase();
            if text.contains("flagged for possible cybersecurity risk")
                || text.contains("content policy")
                || text.contains("content_policy")
            {
                FailureKind::ProviderRefused
            } else if crate::management::v2::quota_continue::quota_error_text(message) {
                if message.contains("(rate_limit)") {
                    FailureKind::RateLimited
                } else {
                    FailureKind::Quota
                }
            } else {
                FailureKind::Other
            }
        }
    };
    HandlerFailure {
        auto_requeued: false,
        resumes_at: None,
        resets_at: error
            .get("resetsAt")
            .or_else(|| error.get("resetAt"))
            .and_then(|v| {
                v.as_str()
                    .and_then(super::parse_time)
                    .or_else(|| v.as_i64().and_then(|at| DateTime::from_timestamp(at, 0)))
            })
            .map(now_rfc3339),
        hold: None,
        gate_closed: false,
        kind,
        message: truncate_chars(message, 1200),
        code: code.map(|c| truncate_chars(c, 120)),
        turn_id: turn_id.map(str::to_owned),
        last_activity: activity.clone(),
        delegation_open: true,
        failed_at: now_rfc3339(now),
    }
}

#[derive(Default)]
pub(super) struct QuotaSchedule {
    pub resumes_at: Option<String>,
    pub resets_at: Option<String>,
}

/// Read quota state before taking the ask-store transaction. An estimated
/// probe is not a known reset or a promise that the next turn will resume then.
pub(super) fn quota_schedule(
    session: &str,
    generation: u64,
    turn_id: Option<&str>,
) -> QuotaSchedule {
    let pending = crate::management::v2::quota_continue::Store::open_default()
        .ok()
        .and_then(|store| store.snapshot().ok())
        .and_then(|state| {
            state
                .sessions
                .get(session)
                .and_then(|entry| entry.pending.clone())
        });
    let Some(pending) = pending.filter(|p| {
        !p.estimated
            && p.runtime_generation == generation
            && p.turn_id.as_deref().is_none_or(|id| Some(id) == turn_id)
    }) else {
        return QuotaSchedule::default();
    };
    QuotaSchedule {
        resumes_at: Some(pending.at),
        resets_at: DateTime::from_timestamp(pending.reset_at, 0).map(now_rfc3339),
    }
}

/// Called under the existing store transaction after occurrence fencing.
/// Terminal attempt state and the delivery key make duplicate completions inert.
pub(super) fn settle(
    state: &mut AskState,
    session: &str,
    ask_id: &str,
    attempt: u32,
    turn: &Value,
    runtime_lost: bool,
    schedule: &QuotaSchedule,
    now: DateTime<Utc>,
) -> bool {
    if !super::observe::attempt_open(state, ask_id, attempt) {
        return false;
    }
    let ask = &state.asks[ask_id];
    // Peer v1 only has stop/close operations, not resume/reopen. Preserve its
    // previous failed-answer/close contract until those wire controls exist.
    if ask.peer_inbound.is_some() {
        return false;
    }
    let Some(record) = ask.attempts.iter().find(|r| r.n == attempt) else {
        return false;
    };
    if record.stop_requested || owner_stopped(state, session, record) {
        return false;
    }
    if turn.get("status").and_then(Value::as_str) != Some("failed") && !runtime_lost {
        return false;
    }
    let mut failure = turn.get("error").filter(|e| !e.is_null()).map(|error|
        diagnostic(error, record.turn_id.as_deref(), &record.activity, now))
        .or_else(|| record.handler_failure.clone())
        .unwrap_or_else(||diagnostic(&json!({"code":if runtime_lost {"runtimeExited"} else {"other"},
            "message":if runtime_lost {"The handler runtime ended before its turn could finish."} else {"Handler turn failed"}}),
            record.turn_id.as_deref(), &record.activity, now));
    failure.last_activity = record.activity.clone();
    failure.failed_at = now_rfc3339(now);
    failure.gate_closed = state.pauses.contains_key(session);
    if failure.resets_at.is_none() {
        failure.resets_at = schedule.resets_at.clone();
    }
    if turn.get("error").is_none_or(|error| code(error).is_none()) {
        if let Some(cached) = &record.handler_failure {
            if failure.kind == FailureKind::Other && cached.kind != FailureKind::Other {
                failure.kind = cached.kind;
                failure.code = cached.code.clone();
                failure.message = cached.message.clone();
            }
        }
    }
    let generation = record.runtime_generation;
    let turn_id = record.turn_id.clone();
    state.update_ask(ask_id, now, |ask| {
        let record = ask.attempts.iter_mut().find(|r| r.n == attempt).unwrap();
        record.handler_failure = Some(failure.clone());
        record.last_error = Some(failure.message.clone());
        record.ended_at = Some(now_rfc3339(now));
        record.state = if runtime_lost {
            AttemptState::Interrupted
        } else {
            AttemptState::Failed
        };
        if runtime_lost {
            record.interrupt_reason = Some(InterruptReason::RuntimeRestart);
        }
        record.compaction_reminder_pending = false;
        ask.execution = if runtime_lost {
            ExecutionState::Interrupted
        } else {
            ExecutionState::Failed
        };
        ask.hold = None;
        ask.awaiting = None;
        ask.resume = None;
        ask.bounded_continuation.cancel("handler_turn_failed");
    });
    for question in state
        .questions
        .values_mut()
        .filter(|q| q.ask_id == ask_id && q.attempt == attempt && q.state == QuestionState::Open)
    {
        question.state = QuestionState::Stale;
    }
    if state.leases.get(session).is_some_and(|lease|
        matches!(&lease.holder, LeaseHolder::Ask { ask_id: id, attempt: n } if id == ask_id && *n == attempt)
            && lease.runtime_generation == generation && lease.turn_id == turn_id) {
        state.leases.remove(session);
    }
    let ask = (*state.asks[ask_id]).clone();
    super::delivery::record_handler_failure(state, &ask, attempt, now);
    true
}

pub(super) fn owner_stopped(state: &AskState, session: &str, record: &AttemptRecord) -> bool {
    state.pauses.get(session).is_some_and(|pause| {
        pause.origin.as_deref() == Some(super::control::ORIGIN_OWNER_STOP)
            && super::parse_time(&pause.since)
                // turn/started may be delivered after Stop. Admission intent is
                // durable before either notification and identifies this attempt.
                .zip(super::parse_time(&record.submitted_at))
                .is_some_and(|(stopped, started)| stopped >= started)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::{
        observe::observe,
        scheduler::{
            tests::{world, FakeRuntime},
            tick,
        },
        store::tests::{new_ask, temp_store, REQUESTER, TARGET},
    };

    struct ExitRuntime {
        inner: FakeRuntime,
        exited: std::cell::Cell<bool>,
    }
    impl crate::asks::scheduler::AskRuntime for ExitRuntime {
        fn target_status(&self, s: &str) -> crate::asks::scheduler::TargetStatus {
            self.inner.target_status(s)
        }
        fn runtime_exited(&self, _: &str, _: u64) -> bool {
            self.exited.get()
        }
        fn submit_exclusive(
            &self,
            s: &str,
            g: u64,
            input: &crate::asks::scheduler::Submission,
        ) -> crate::asks::scheduler::SubmitOutcome {
            self.inner.submit_exclusive(s, g, input)
        }
        fn message_status(
            &self,
            s: &str,
            g: u64,
            input: &crate::asks::scheduler::Submission,
        ) -> crate::asks::scheduler::NativeMessageStatus {
            self.inner.message_status(s, g, input)
        }
        fn interrupt_keep_gate(&self, s: &str, t: &str) -> anyhow::Result<()> {
            self.inner.interrupt_keep_gate(s, t)
        }
        fn resume_gate(&self, s: &str) -> anyhow::Result<()> {
            self.inner.resume_gate(s)
        }
        fn read_turn(&self, s: &str, t: &str) -> Option<Value> {
            self.inner.read_turn(s, t)
        }
        fn respond_question(
            &self,
            s: &str,
            g: u64,
            id: &str,
            req: &Value,
            result: &Value,
        ) -> crate::asks::scheduler::RespondOutcome {
            self.inner.respond_question(s, g, id, req, result)
        }
    }

    #[test]
    fn proven_runtime_exit_notifies_but_disconnect_alone_does_not() {
        use crate::asks::scheduler::{reconcile_bound, TickReport};
        let (world, dir) = world("failure-exit");
        let (store, store_dir) = temp_store("failure-exit");
        let now = Utc::now();
        let runtime = ExitRuntime {
            inner: FakeRuntime::idle(&[TARGET]),
            exited: std::cell::Cell::new(false),
        };
        let ask = store
            .with_state(|s| {
                s.create_ask(new_ask("failure-exit", "x"), now)
                    .unwrap()
                    .0
                    .ask_id
            })
            .unwrap();
        tick(&world, &runtime, &store, now).unwrap();
        let lease = store.inspect(|s| s.leases[TARGET].clone()).unwrap();
        runtime.inner.set(TARGET, |status| {
            status.online = false;
            status.active_turn_id = None;
        });
        let mut report = TickReport::default();
        reconcile_bound(&runtime, &store, &lease, &mut report, now).unwrap();
        store
            .inspect(|s| assert!(!s.deliveries.values().any(|d| d.ref_id.starts_with(PREFIX))))
            .unwrap();
        runtime.exited.set(true);
        reconcile_bound(&runtime, &store, &lease, &mut report, now).unwrap();
        reconcile_bound(&runtime, &store, &lease, &mut report, now).unwrap();
        store
            .inspect(|s| {
                assert!(s.asks[&ask].is_open());
                assert_eq!(
                    s.asks[&ask].attempts[0]
                        .handler_failure
                        .as_ref()
                        .unwrap()
                        .kind,
                    FailureKind::RuntimeError
                );
                assert_eq!(
                    s.deliveries
                        .values()
                        .filter(|d| d.ref_id.starts_with(PREFIX))
                        .count(),
                    1
                );
            })
            .unwrap();
        let _ = std::fs::remove_dir_all(store_dir);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn structured_codes_override_text_and_bound_provider_message() {
        for (error, expected) in [
            (
                json!({"codexErrorInfo":"cyberPolicy","message":"localized"}),
                FailureKind::ProviderRefused,
            ),
            (
                json!({"codexErrorInfo":"usageLimitExceeded","message":"localized"}),
                FailureKind::Quota,
            ),
            (
                json!({"codexErrorInfo":"rateLimitExceeded","message":"localized 429"}),
                FailureKind::RateLimited,
            ),
            (
                json!({"codexErrorInfo":{"responseStreamDisconnected":{"httpStatusCode":503}},"message":"localized"}),
                FailureKind::RuntimeError,
            ),
            (
                json!({"codexErrorInfo":"badRequest","message":"content policy"}),
                FailureKind::Other,
            ),
            (
                json!({"message":"{\"error\":{\"code\":\"content_policy_violation\",\"message\":\"localized\"}}"}),
                FailureKind::ProviderRefused,
            ),
        ] {
            assert_eq!(
                diagnostic(&error, None, &Activity::default(), Utc::now()).kind,
                expected
            );
        }
        let long = diagnostic(
            &json!({"message":"中".repeat(5000)}),
            None,
            &Activity::default(),
            Utc::now(),
        );
        assert!(long.message.chars().count() <= 1201);
    }

    #[test]
    fn refused_turn_notifies_once_stays_open_then_explicit_continue_completes() {
        let (world, dir) = world("handler-failure");
        let (store, store_dir) = temp_store("handler-failure");
        let runtime = FakeRuntime::idle(&[TARGET, REQUESTER]);
        let now = Utc::now();
        let ask = store
            .with_state(|s| s.create_ask(new_ask("refused", "x"), now).unwrap().0.ask_id)
            .unwrap();
        tick(&world, &runtime, &store, now).unwrap();
        observe(
            &store,
            TARGET,
            "item/started",
            &json!({"turnId":"turn-1","item":{"type":"commandExecution","command":"cargo test"}}),
            now,
        )
        .unwrap();
        observe(&store,TARGET,"error",&json!({"turnId":"turn-1","willRetry":false,"error":{"codexErrorInfo":"cyberPolicy","message":"localized refusal"}}),now).unwrap();
        store
            .inspect(|s| assert!(!s.deliveries.values().any(|d| d.ref_id.starts_with(PREFIX))))
            .unwrap();
        let failure = json!({"turn":{"id":"turn-1","status":"failed","error":null}});
        for _ in 0..2 {
            observe(&store, TARGET, "turn/completed", &failure, now).unwrap();
        }
        store
            .inspect(|s| {
                let a = &s.asks[&ask];
                assert!(a.is_open());
                assert_eq!(a.execution, ExecutionState::Failed);
                assert_eq!(
                    s.notices
                        .iter()
                        .filter(|n| n.params.get("handlerFailure").is_some_and(|v| !v.is_null()))
                        .count(),
                    1
                );
                assert!(!s.leases.contains_key(TARGET));
                assert!(s.answers.is_empty());
                let ds: Vec<_> = s
                    .deliveries
                    .values()
                    .filter(|d| d.ref_id.starts_with(PREFIX))
                    .collect();
                assert_eq!(ds.len(), 1);
                assert!(!ds[0].passive);
                let item = crate::asks::returns::return_item(&store, s, ds[0]).unwrap();
                let value = crate::asks::returns::item_json(&item);
                assert_eq!(value["handlerFailure"]["kind"], "provider_refused");
                assert_eq!(
                    value["handlerFailure"]["lastActivity"]["line"],
                    "running cargo test"
                );
                assert_eq!(value["handlerFailure"]["delegationOpen"], true);
                assert!(crate::asks::render::return_text(&[item])
                    .1
                    .contains("localized refusal"));
            })
            .unwrap();
        runtime.set(TARGET, |status| status.active_turn_id = None);
        store
            .with_state(|s| {
                crate::asks::awaits::resume_now(s, &ask, Some(REQUESTER), Some("continue"), now)
                    .unwrap()
            })
            .unwrap();
        tick(&world, &runtime, &store, now).unwrap();
        let turn = store
            .inspect(|s| {
                assert_eq!(s.asks[&ask].attempts.len(), 2);
                assert_eq!(
                    s.notices
                        .iter()
                        .filter(|n| n.params.get("handlerFailure").is_some_and(|v| !v.is_null()))
                        .count(),
                    1
                );
                s.asks[&ask]
                    .current_attempt()
                    .unwrap()
                    .turn_id
                    .clone()
                    .unwrap()
            })
            .unwrap();
        assert!(runtime.submitted.borrow().iter().any(
            |(session, input)| session == REQUESTER && input.text.contains("localized refusal")
        ));
        observe(&store,TARGET,"turn/completed",&json!({"turn":{"id":turn,"status":"completed","items":[{"type":"agentMessage","phase":"final_answer","text":"done"}]}}),now).unwrap();
        store
            .inspect(|s| {
                assert_eq!(s.asks[&ask].execution, ExecutionState::Answered);
                assert_eq!(s.answers.len(), 1);
            })
            .unwrap();
        let _ = std::fs::remove_dir_all(store_dir);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn quota_passive_error_event_and_owner_stop() {
        for (label, stop, passive, quota_requeue) in [
            ("quota", false, false, false),
            ("passive", false, true, false),
            ("stop", true, false, false),
            ("stop_failed", true, false, true),
            ("stop_late_start", true, false, true),
            ("autoquota", false, false, true),
        ] {
            let (world, dir) = world(label);
            let (store, store_dir) = temp_store(label);
            let runtime = FakeRuntime::idle(&[TARGET]);
            let now = Utc::now();
            let ask = store
                .with_state(|s| {
                    let mut new = new_ask(label, "x");
                    if passive {
                        new.options.notify = Notify::Passive;
                    }
                    s.create_ask(new, now).unwrap().0.ask_id
                })
                .unwrap();
            tick(&world, &runtime, &store, now).unwrap();
            if stop {
                store
                    .with_state(|s| crate::asks::control::owner_stop(s, TARGET, now))
                    .unwrap();
            }
            if label == "stop_late_start" {
                observe(
                    &store,
                    TARGET,
                    "turn/started",
                    &json!({"turn":{"id":"turn-1"}}),
                    now + chrono::Duration::seconds(1),
                )
                .unwrap();
            }
            observe(&store,TARGET,"error",&json!({"turnId":"turn-1","willRetry":false,"error":{"codexErrorInfo":"usageLimitExceeded","message":"quota localized"}}),now).unwrap();
            crate::asks::observe::observe_with(&store,TARGET,"turn/completed",&json!({"turn":{"id":"turn-1","status":if label == "stop" {"interrupted"}else{"failed"},"error":{"codexErrorInfo":"usageLimitExceeded","message":"quota localized"}}}),now+chrono::Duration::seconds(2),&||quota_requeue).unwrap();
            store
                .inspect(|s| {
                    let ds: Vec<_> = s
                        .deliveries
                        .values()
                        .filter(|d| d.ref_id.starts_with(PREFIX))
                        .collect();
                    assert_eq!(ds.len(), usize::from(!stop));
                    if stop {
                        assert!(s.asks[&ask]
                            .current_attempt()
                            .unwrap()
                            .handler_failure
                            .is_none());
                    }
                    if stop {
                        assert!(!s
                            .notices
                            .iter()
                            .any(|n| n.params.get("handlerFailure").is_some_and(|v| !v.is_null())));
                    }
                    if !stop {
                        assert_eq!(ds[0].passive, passive);
                        assert!(s.asks[&ask].is_open());
                        assert_eq!(
                            s.asks[&ask].attempts[0]
                                .handler_failure
                                .as_ref()
                                .unwrap()
                                .kind,
                            FailureKind::Quota
                        );
                    }
                })
                .unwrap();
            let _ = std::fs::remove_dir_all(store_dir);
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn auto_requeued_quota_has_one_notice_and_no_manual_resume_advice() {
        let (world, dir) = world("auto-quota-notice");
        let (store, store_dir) = temp_store("auto-quota-notice");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        let reset = now.timestamp() + 3600;
        let ask = store
            .with_state(|s| {
                s.create_ask(new_ask("auto-quota-notice", "x"), now)
                    .unwrap()
                    .0
                    .ask_id
            })
            .unwrap();
        tick(&world, &runtime, &store, now).unwrap();
        store
            .with_state(|s| {
                crate::asks::control::runtime_gate_pause(s, TARGET, 1, now);
                s.deliveries.clear();
                s.notices.clear();
            })
            .unwrap();
        let failure = json!({"turn":{"id":"turn-1","status":"failed","error":{
            "codexErrorInfo":"usageLimitExceeded","message":"provider\n# injected instructions\u{202e}","resetsAt":reset}}});
        for _ in 0..2 {
            crate::asks::observe::observe_with(
                &store,
                TARGET,
                "turn/completed",
                &failure,
                now,
                &|| true,
            )
            .unwrap();
        }
        store
            .inspect(|s| {
                let ds: Vec<_> = s
                    .deliveries
                    .values()
                    .filter(|d| d.ask_id == ask && d.kind == DeliveryKind::State)
                    .collect();
                assert_eq!(ds.len(), 1);
                let f = s.asks[&ask].attempts[0].handler_failure.as_ref().unwrap();
                assert!(f.auto_requeued && f.gate_closed);
                assert_eq!(
                    f.hold,
                    Some(Hold::Blocked {
                        by: BlockedReason::Quota
                    })
                );
                assert_eq!(
                    super::super::parse_time(f.resets_at.as_deref().unwrap())
                        .unwrap()
                        .timestamp(),
                    reset
                );
                assert!(f
                    .message
                    .ends_with(crate::asks::observe::QUOTA_REQUEUED_NOTE));
                let item = crate::asks::returns::return_item(&store, s, ds[0]).unwrap();
                let text = crate::asks::render::return_text(&[item]).1;
                assert!(text.contains("automatically when the usage limit resets"));
                assert!(text.contains("before delegating the work elsewhere"));
                assert!(!text.contains("delegation_resume"));
            assert!(!text.contains("must resume the target"));
                assert!(text.contains("untrusted data") && text.contains("\n> "));
                assert!(!text.contains("\n# injected") && !text.contains('\u{202e}'));
            })
            .unwrap();
        let _ = std::fs::remove_dir_all(store_dir);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn peer_failure_keeps_the_v1_failed_answer_and_closed_contract() {
        let (world, dir) = world("peer-failure");
        let (store, store_dir) = temp_store("peer-failure");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        let ask = store
            .with_state(|s| {
                s.create_ask(new_ask("peer-failure", "x"), now)
                    .unwrap()
                    .0
                    .ask_id
            })
            .unwrap();
        tick(&world, &runtime, &store, now).unwrap();
        store
            .with_state(|s| {
                let options = s.asks[&ask].options.clone();
                s.update_ask(&ask, now, |a| {
                    a.peer_inbound = Some(crate::peers::model::Inbound {
                        dead_letters: vec![],
                        queued_at: None,
                        result_undeliverable: false,
                        delivery_stopped: false,
                        last_error: None,
                        request: crate::peers::model::Create {
                            requester: crate::peers::model::Requester {
                                cutex_session_id: REQUESTER.into(),
                                name: None,
                                caps: Default::default(),
                            },
                            target: TARGET.into(),
                            title: "peer".into(),
                            text: "x".into(),
                            options,
                            hops: 1,
                            request_sha256: "x".into(),
                        },
                        peer_deliveries: vec![],
                        received_ops: vec![],
                        next_attempt_at: None,
                        failures: 0,
                    })
                });
            })
            .unwrap();
        observe(&store,TARGET,"turn/completed",&json!({"turn":{"id":"turn-1","status":"failed", "error":{"codexErrorInfo":"cyberPolicy","message":"refused"}}}),now).unwrap();
        store
            .inspect(|s| {
                assert_eq!(s.asks[&ask].closed, Some(CloseOutcome::Completed));
                assert_eq!(s.answers.len(), 1);
                assert!(!s.deliveries.values().any(|d| d.ref_id.starts_with(PREFIX)));
            })
            .unwrap();
        let _ = std::fs::remove_dir_all(store_dir);
        let _ = std::fs::remove_dir_all(dir);
    }
}
