//! Idle result batching. Deadlines derive from durable pending-delivery times,
//! so scheduler restarts and a stream of results cannot reset the hard cap.
use super::model::{AnswerStatus, DeliveryKind};
use super::store::AskState;
use crate::config::result_coalescing::{ResultClass, ResultCoalescing};
use chrono::{DateTime, Duration, Utc};

// Only idle result batching uses this deadline; busy insertion has its own
// scheduler policy. Persisted first arrival anchors the cap across restarts.
const IDLE_MAX_WAIT_SECS: u64 = 5;

#[derive(Clone, Copy, Debug)]
pub struct Context {
    pub class: ResultClass,
    pub last_activity: Option<DateTime<Utc>>,
}

/// Shared by scheduler admission and its owner-visible projection. Read only
/// the selected session's current generation; never infer a cache from age.
pub fn context_for_session(session: &str) -> Option<Context> {
    let sessions = crate::session::store::load_cutex_session_store_shared().ok()?;
    let record = sessions.sessions.get(session)?;
    let activity = crate::management::v2::activity::try_load_session_activity_states()
        .ok().flatten().and_then(|states| states.get(session).cloned());
    let usage = record.claude_session.as_ref().and_then(|binding| {
        crate::management::v2::claude_token_usage::latest(session, &binding.thread_id).ok().flatten()
    });
    Some(Context::from_record(record, activity.as_ref())
        .with_cache_usage(record.runtime_generation, usage.as_ref()))
}

/// Query numbered input only for a batch that would actually wait. Due,
/// disabled and urgent batches do not need a queue projection.
pub(crate) fn hold_unless_numbered_input(
    would_hold: bool,
    has_input: impl FnOnce() -> anyhow::Result<bool>,
) -> anyhow::Result<bool> {
    Ok(would_hold && !has_input()?)
}

impl Context {
    /// Usage must have been loaded for the session's bound Claude thread.
    /// A subscription account using extra/API usage can have the five-minute
    /// cache policy. Malformed, legacy and other-occurrence reports cannot
    /// manufacture that evidence; API/other accounts never become subscription.
    pub fn with_cache_usage(mut self, generation: u64, usage: Option<&serde_json::Value>) -> Self {
        if self.class == ResultClass::ClaudeSubscription
            && usage.is_some_and(|usage| {
                usage
                    .get("runtimeGeneration")
                    .and_then(serde_json::Value::as_u64)
                    == Some(generation)
                    && usage
                        .pointer("/tokenUsage/cacheTtlSeconds")
                        .and_then(serde_json::Value::as_u64)
                        == Some(300)
            })
        {
            self.class = ResultClass::ClaudeOther;
        }
        self
    }

    pub fn from_record(
        record: &crate::session::model::CutexSessionRecord,
        activity: Option<&crate::management::v2::activity::SessionActivityState>,
    ) -> Self {
        use crate::session::model::CutexSessionRuntimeBackend;
        let class = if record.runtime_backend == CutexSessionRuntimeBackend::ClaudeSubscription {
            ResultClass::ClaudeSubscription
        } else if record.claude_session.is_some() {
            ResultClass::ClaudeOther
        } else {
            match record.runtime_backend {
                CutexSessionRuntimeBackend::Host
                | CutexSessionRuntimeBackend::HostForeground
                | CutexSessionRuntimeBackend::Docker => ResultClass::Codex,
                _ => ResultClass::ClaudeOther,
            }
        };
        Self {
            class,
            // Persisted turn completion survives management restarts. Missing or
            // old-generation evidence remains unknown; presence/metadata cannot
            // manufacture a warm cache or release a busy batch.
            last_activity: activity
                .filter(|s| {
                    s.runtime_generation == Some(record.runtime_generation)
                        && s.last_turn_completed_generation == Some(record.runtime_generation)
                })
                .and_then(|s| s.last_turn_completed_at.as_deref())
                .and_then(super::parse_time),
        }
    }
}

/// Questions, failure/interruption answers, and state transitions never wait
/// behind regular successful answers (nor behind their quiet window).
pub(crate) fn urgent(state: &AskState, requester: &str) -> bool {
    state.deliveries.values().any(|d| {
        !d.passive
            && super::delivery::actionable(state, d)
            && super::delivery::is_for(state, d, requester)
            && match d.kind {
                DeliveryKind::Answer => state
                    .answers
                    .get(&d.ref_id)
                    .is_none_or(|a| a.status != AnswerStatus::Completed),
                DeliveryKind::Question | DeliveryKind::State => true,
            }
    })
}

fn deadline(
    context: Context,
    settings: ResultCoalescing,
    oldest: DateTime<Utc>,
    newest: DateTime<Utc>,
) -> DateTime<Utc> {
    let window = settings.window(context.class);
    if !window.enabled {
        return oldest;
    }
    let warm_secs = match context.class {
        ResultClass::ClaudeSubscription => Some(3300),
        ResultClass::ClaudeOther => Some(270),
        ResultClass::Codex => None,
    };
    let warm_end = context
        .last_activity
        .zip(warm_secs)
        .map(|(at, secs)| at + Duration::seconds(secs));
    // Choose the cold rule at the batch's arrival, not when a later tick crosses
    // the warm deadline. Otherwise a warm batch could be postponed past expiry.
    let cold_secs = match context.class {
        ResultClass::ClaudeSubscription => Some(3300),
        ResultClass::ClaudeOther => Some(300),
        ResultClass::Codex => None,
    };
    let cold = context
        .last_activity
        .zip(cold_secs)
        .is_some_and(|(at, secs)| oldest >= at + Duration::seconds(secs));
    let (quiet, cap) = if cold {
        (window.cold_quiet_secs, window.cold_max_wait_secs)
    } else {
        (window.quiet_secs, window.max_wait_secs)
    };
    let mut due = (newest + Duration::seconds(quiet.min(IDLE_MAX_WAIT_SECS) as i64))
        .min(oldest + Duration::seconds(cap.min(IDLE_MAX_WAIT_SECS) as i64));
    if !cold {
        if let Some(end) = warm_end {
            due = due.min(end);
        }
    }
    due
}

pub(crate) fn hold(
    state: &AskState,
    requester: &str,
    context: Context,
    settings: ResultCoalescing,
    now: DateTime<Utc>,
) -> bool {
    release_at(state, requester, context, settings, now).is_some()
}

/// The actual scheduler deadline, including urgent/activity overrides and the
/// durable oldest-result cap. None means that this batch must not wait.
pub(crate) fn release_at(
    state: &AskState, requester: &str, context: Context,
    settings: ResultCoalescing, now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    if urgent(state, requester) {
        return None;
    }
    let mut times = state
        .deliveries
        .values()
        .filter(|d| {
            !d.passive
                && super::delivery::actionable(state, d)
                && super::delivery::is_for(state, d, requester)
        })
        .filter_map(|d| super::parse_time(&d.created_at));
    let Some(first) = times.next() else {
        return None;
    };
    let (oldest, newest) = times.fold((first, first), |(old, new), at| (old.min(at), new.max(at)));
    // Existing activity after the first result (in particular a busy turn's
    // completion) consumes the busy wait already; deliver that batch at once.
    if context.last_activity.is_some_and(|at| at >= oldest) {
        return None;
    }
    let due = deadline(context, settings, oldest, newest);
    (now < due).then_some(due)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn at(s: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(s, 0).unwrap()
    }
    #[test]
    fn due_or_disabled_batches_do_not_read_the_queue() {
        assert!(!hold_unless_numbered_input(false, || panic!("unnecessary queue read")).unwrap());
        let reads = std::cell::Cell::new(0);
        for pending_input in [true, false] {
            assert_eq!(
                hold_unless_numbered_input(true, || {
                    reads.set(reads.get() + 1);
                    Ok(pending_input)
                })
                .unwrap(),
                !pending_input
            );
        }
        assert_eq!(reads.get(), 2);
    }
    #[test]
    fn backend_classes_and_existing_activity_are_conservative() {
        use crate::session::model::{CutexSessionRecord, CutexSessionRuntimeBackend};
        let mut record = CutexSessionRecord::new_at(
            "cutex.00000000-0000-0000-0000-000000000001".into(),
            None,
            "test".into(),
            "/tmp".into(),
            None,
            at(0).to_rfc3339(),
        )
        .unwrap();
        record.last_seen_at = Some(at(12).to_rfc3339());
        assert_eq!(
            Context::from_record(&record, None).class,
            ResultClass::Codex
        );
        record.runtime_backend = CutexSessionRuntimeBackend::ClaudeSubscription;
        let mut activity = crate::management::v2::activity::SessionActivityState {
            runtime_generation: Some(record.runtime_generation),
            last_turn_completed_generation: Some(record.runtime_generation),
            last_turn_completed_at: Some(at(20).to_rfc3339()),
            ..Default::default()
        };
        let context = Context::from_record(&record, Some(&activity));
        assert_eq!(context.class, ResultClass::ClaudeSubscription);
        assert_eq!(context.last_activity, Some(at(20)));
        // Unrelated runtime activity must not replace the completion clock.
        activity.last_output_at = Some(at(300).to_rfc3339());
        activity.last_conversation_at = Some(at(300).to_rfc3339());
        assert_eq!(
            Context::from_record(&record, Some(&activity)).last_activity,
            Some(at(20))
        );
        let restored = serde_json::from_value(serde_json::to_value(&activity).unwrap()).unwrap();
        assert_eq!(
            Context::from_record(&record, Some(&restored)).last_activity,
            Some(at(20))
        );
        activity.runtime_generation = Some(record.runtime_generation + 1);
        assert_eq!(
            Context::from_record(&record, Some(&activity)).last_activity,
            None
        );
        assert_eq!(Context::from_record(&record, None).last_activity, None);
        record.runtime_generation += 1;
        assert_eq!(
            Context::from_record(&record, Some(&activity)).last_activity,
            None,
            "completion belongs to the previous generation"
        );
        record.runtime_backend = CutexSessionRuntimeBackend::Future;
        assert_eq!(
            Context::from_record(&record, None).class,
            ResultClass::ClaudeOther
        );
    }
    #[test]
    fn idle_warm_cold_deadlines_are_capped_at_five_seconds() {
        let defaults = ResultCoalescing::default();
        let c = |class, last| Context {
            class,
            last_activity: Some(at(last)),
        };
        assert_eq!(
            deadline(
                c(ResultClass::ClaudeSubscription, 0),
                defaults,
                at(100),
                at(140)
            ),
            at(105)
        );
        assert_eq!(
            deadline(c(ResultClass::ClaudeOther, 0), defaults, at(260), at(265)),
            at(265)
        );
        assert_eq!(
            deadline(c(ResultClass::ClaudeOther, 0), defaults, at(280), at(285)),
            at(270)
        );
        assert_eq!(
            deadline(c(ResultClass::ClaudeOther, 0), defaults, at(300), at(400)),
            at(305)
        );
        assert_eq!(
            deadline(c(ResultClass::ClaudeOther, 0), defaults, at(300), at(300)),
            at(305)
        );
        assert_eq!(
            deadline(
                c(ResultClass::ClaudeSubscription, 0),
                defaults,
                at(3300),
                at(3400)
            ),
            at(3305)
        );
        assert_eq!(
            deadline(
                c(ResultClass::ClaudeSubscription, 0),
                defaults,
                at(100),
                at(690)
            ),
            at(105)
        );
        assert_eq!(
            deadline(c(ResultClass::Codex, 0), defaults, at(100), at(215)),
            at(105)
        );
        let mut disabled = defaults;
        disabled.codex.enabled = false;
        assert_eq!(
            deadline(c(ResultClass::Codex, 0), disabled, at(100), at(101)),
            at(100)
        );
    }

    #[test]
    fn ongoing_idle_results_cannot_extend_the_first_arrivals_deadline() {
        for class in [ResultClass::ClaudeSubscription, ResultClass::ClaudeOther, ResultClass::Codex] {
            let context = Context { class, last_activity: None };
            for newest in 100..=120 {
                assert_eq!(deadline(context, ResultCoalescing::default(), at(100), at(newest)), at(105));
            }
        }
        let context = Context { class: ResultClass::Codex, last_activity: None };
        let mut settings = ResultCoalescing::default();
        settings.codex.quiet_secs = 0;
        assert_eq!(deadline(context, settings, at(100), at(100)), at(100), "zero quiet still releases immediately");
        settings.codex.quiet_secs = 2;
        assert_eq!(deadline(context, settings, at(100), at(101)), at(103), "shorter configured windows still apply");
        settings.codex.max_wait_secs = 0;
        assert_eq!(deadline(context, settings, at(100), at(101)), at(100), "zero cap never waits");
    }

    #[test]
    fn unrelated_activity_and_restart_keep_an_idle_batch_held() {
        use crate::asks::model::{AnswerMetadata, AnswerRecord};
        use crate::asks::store::tests::{new_ask, REQUESTER};
        let mut record = crate::session::model::CutexSessionRecord::new_at(
            REQUESTER.into(),
            None,
            "test".into(),
            "/tmp".into(),
            None,
            at(0).to_rfc3339(),
        )
        .unwrap();
        record.runtime_backend =
            crate::session::model::CutexSessionRuntimeBackend::ClaudeSubscription;
        record.runtime_generation = 1;
        let mut activity = crate::management::v2::activity::SessionActivityState {
            runtime_generation: Some(1),
            last_turn_completed_generation: Some(1),
            last_turn_completed_at: Some(at(20).to_rfc3339()),
            ..Default::default()
        };
        let mut state = AskState::default();
        let ask = state
            .create_ask(new_ask("held", "result"), at(100))
            .unwrap()
            .0;
        state.answers.insert(
            "answer".into(),
            AnswerRecord {
                answer_id: "answer".into(),
                ask_id: ask.ask_id.clone(),
                attempt: 1,
                revision: 1,
                status: AnswerStatus::Completed,
                artifact_id: None,
                bytes: 0,
                metadata: AnswerMetadata::default(),
                created_at: at(100).to_rfc3339(),
                preview: None,
            },
        );
        super::super::delivery::record_answer_delivery(&mut state, &ask, "answer", at(100))
            .unwrap();
        let settings = ResultCoalescing::default();
        let context = Context::from_record(&record, Some(&activity));
        let mut rows = crate::management::v2::admission::ask_next_step(&state, REQUESTER);
        crate::management::v2::admission::project_result_window(
            &mut rows, &state, REQUESTER, context, settings, at(104), false);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].wait_reason.as_deref(), Some("resultCoalescing"));
        assert_eq!(super::super::parse_time(rows[0].next_step_at.as_deref().unwrap()), Some(at(105)));
        let mut custom = settings;
        custom.claude_subscription.quiet_secs = 2;
        let mut configured = crate::management::v2::admission::ask_next_step(&state, REQUESTER);
        crate::management::v2::admission::project_result_window(
            &mut configured, &state, REQUESTER, context, custom, at(101), false);
        assert_eq!(super::super::parse_time(configured[0].next_step_at.as_deref().unwrap()), Some(at(102)),
            "projection uses actual settings, not a guessed default window");
        // A displayed deadline and scheduler release share the same boundary:
        // no owner message is required once this window elapses.
        assert_eq!(release_at(&state, REQUESTER, context, settings, at(104)), Some(at(105)));
        assert!(!hold(&state, REQUESTER, context, settings, at(105)));
        let mut bypass = crate::management::v2::admission::ask_next_step(&state, REQUESTER);
        crate::management::v2::admission::project_result_window(
            &mut bypass, &state, REQUESTER, context, settings, at(104), true);
        assert!(bypass[0].wait_reason.is_none(), "numbered input ends batching early");
        assert!(hold(
            &state,
            REQUESTER,
            Context::from_record(&record, Some(&activity)),
            settings,
            at(104)
        ));
        activity.last_output_at = Some(at(104).to_rfc3339());
        record.last_seen_at = Some(at(104).to_rfc3339());
        assert!(hold(
            &state,
            REQUESTER,
            Context::from_record(&record, Some(&activity)),
            settings,
            at(104)
        ));
        let restored = serde_json::from_value(serde_json::to_value(activity).unwrap()).unwrap();
        assert!(hold(
            &state,
            REQUESTER,
            Context::from_record(&record, Some(&restored)),
            settings,
            at(104)
        ));
        assert!(!hold(&state, REQUESTER, Context { last_activity: Some(at(100)), ..context }, settings, at(101)),
            "completed activity already consumed the busy wait");
        let mut disabled = settings;
        disabled.claude_subscription.enabled = false;
        assert!(!hold(&state, REQUESTER, context, disabled, at(101)));
        state.answers.get_mut("answer").unwrap().status = AnswerStatus::Interrupted;
        assert!(!hold(&state, REQUESTER, context, settings, at(101)), "interrupted answers remain urgent");
    }
    #[test]
    fn actual_five_minute_usage_changes_subscription_windows_only_for_current_occurrence() {
        use serde_json::json;
        let context = Context {
            class: ResultClass::ClaudeSubscription,
            last_activity: Some(at(0)),
        };
        let short = json!({"runtimeGeneration":7,"tokenUsage":{"cacheTtlSeconds":300}});
        let adjusted = context.with_cache_usage(7, Some(&short));
        assert_eq!(adjusted.class, ResultClass::ClaudeOther);
        assert_eq!(
            deadline(adjusted, ResultCoalescing::default(), at(260), at(265)),
            at(265)
        );
        assert_eq!(
            deadline(adjusted, ResultCoalescing::default(), at(300), at(300)),
            at(305)
        );
        for invalid in [
            json!({"runtimeGeneration":6,"tokenUsage":{"cacheTtlSeconds":300}}),
            json!({"tokenUsage":{"cacheTtlSeconds":300}}),
            json!({"runtimeGeneration":7,"tokenUsage":{"cacheTtlSeconds":"300"}}),
            json!({"runtimeGeneration":7,"tokenUsage":{"cacheTtlSeconds":3600}}),
        ] {
            assert_eq!(
                context.with_cache_usage(7, Some(&invalid)).class,
                ResultClass::ClaudeSubscription
            );
        }
        assert_eq!(
            context.with_cache_usage(7, None).class,
            ResultClass::ClaudeSubscription
        );
        let other = Context {
            class: ResultClass::ClaudeOther,
            ..context
        };
        assert_eq!(
            other
                .with_cache_usage(
                    7,
                    Some(&json!({"runtimeGeneration":7,"tokenUsage":{"cacheTtlSeconds":3600}}))
                )
                .class,
            ResultClass::ClaudeOther
        );
    }
}
