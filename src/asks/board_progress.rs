//! r290 P6: display-only, evidence-bound progress and quiet/stalled decisions.
use super::model::{AskRecord, ExecutionState};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

pub const STALLED_AFTER_SECONDS: i64 = 900;
fn timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|v| v.with_timezone(&Utc))
}
fn line(value: &str) -> String {
    value.chars().take(300).collect()
}

pub fn plan_progress(plan: &Value) -> Option<Value> {
    // Compact trusted event projection (the factory binds thread/generation).
    if let Some(progress) = plan.get("boardProgress") {
        let steps:super::model::StepProgress=serde_json::from_value(json!({"done":progress["done"],"total":progress["total"],"currentStep":progress["currentStep"]})).ok()?;
        steps.validate().ok()?;
        return Some(
            json!({"done":steps.done,"total":steps.total,"currentStep":steps.current_step,"updatedAt":plan["updatedAt"],"source":"plan"}),
        );
    }
    let steps = plan.get("plan").or_else(|| plan.get("steps"))?.as_array()?;
    if steps.len() > 10000 {
        return None;
    }
    for step in steps {
        step["step"].as_str()?;
        if !matches!(
            step["status"].as_str()?,
            "pending" | "inProgress" | "in_progress" | "completed"
        ) {
            return None;
        }
    }
    let current = steps
        .iter()
        .find(|s| matches!(s["status"].as_str(), Some("inProgress" | "in_progress")))
        .or_else(|| steps.iter().find(|s| s["status"] == "pending"))
        .and_then(|s| s["step"].as_str())
        .map(line);
    Some(
        json!({"done":steps.iter().filter(|s|s["status"]=="completed").count(),"total":steps.len(),"currentStep":current,
        "updatedAt":plan["updatedAt"],"source":"plan"}),
    )
}

pub fn delegation(
    ask: &AskRecord,
    session: Option<&Value>,
    plan: Option<&Value>,
    now: DateTime<Utc>,
) -> Value {
    let Some(attempt) = ask.current_attempt() else {
        return json!({"steps":null,"quietSince":null,"stalled":false,"stallReason":null,"toolCallStartedAt":null,"stalledAfterSeconds":STALLED_AFTER_SECONDS});
    };
    let start = attempt
        .started_at
        .as_deref()
        .and_then(timestamp)
        .or_else(|| timestamp(&attempt.submitted_at));
    let note = attempt
        .progress
        .iter()
        .rev()
        .find(|n| n.steps.as_ref().is_some_and(|s| s.validate().is_ok()));
    let mut steps=note.map(|n| {let s=n.steps.as_ref().unwrap();json!({"done":s.done,"total":s.total,"currentStep":s.current_step,"updatedAt":n.at,"source":"progressNote"})});
    let mut plan_at = None;
    if let Some(plan) = plan.filter(|p| {
        attempt
            .turn_id
            .as_deref()
            .is_some_and(|id| p["turnId"] == id)
            && session
                .and_then(|s| s.pointer("/native/threadId"))
                .is_some_and(|id| p["threadId"] == *id)
    }) {
        plan_at = plan["updatedAt"].as_str().and_then(timestamp);
        if let Some(candidate) = plan_progress(plan) {
            let newer = steps
                .as_ref()
                .and_then(|s| s["updatedAt"].as_str())
                .and_then(timestamp)
                .is_none_or(|at| {
                    candidate["updatedAt"]
                        .as_str()
                        .and_then(timestamp)
                        .is_some_and(|other| other > at)
                });
            if newer {
                steps = Some(candidate);
            }
        }
    }
    let bound = session.is_some_and(|s| {
        s.pointer("/runtime/runtimeGeneration")
            .and_then(Value::as_u64)
            == Some(attempt.runtime_generation)
            && attempt.turn_id.as_deref().is_some_and(|id| {
                s.pointer("/runtime/activeTurnId").and_then(Value::as_str) == Some(id)
            })
    });
    let activity = attempt
        .activity
        .last_activity_at
        .as_deref()
        .and_then(timestamp);
    let mut signals = attempt
        .progress
        .iter()
        .filter_map(|n| timestamp(&n.at))
        .chain(start)
        .chain(activity)
        .chain(plan_at)
        .collect::<Vec<_>>();
    if bound {
        signals.extend(
            session
                .and_then(|s| s.pointer("/activity/lastConversationAt"))
                .and_then(Value::as_str)
                .and_then(timestamp),
        );
        signals.extend(
            session
                .and_then(|s| s.pointer("/activity/lastOutputAt"))
                .and_then(Value::as_str)
                .and_then(timestamp),
        );
    }
    let quiet = signals
        .into_iter()
        .filter(|at| *at <= now && start.is_none_or(|s| *at >= s))
        .max();
    let online = session
        .is_some_and(|s| s.pointer("/runtime/status").and_then(Value::as_str) == Some("online"));
    let paused = session
        .and_then(|s| s["pauseReasons"].as_array())
        .is_some_and(|p| !p.is_empty());
    let eligible = ask.is_open()
        && matches!(
            ask.execution,
            ExecutionState::Running | ExecutionState::Admitted
        )
        && !attempt.state.is_terminal()
        && ask.hold.is_none()
        && !ask.is_waiting()
        && !ask.resume_pending()
        && !attempt.stop_requested
        && !paused
        && online
        && bound;
    let tool_started = eligible
        .then(|| {
            attempt
                .activity
                .in_flight_tools
                .values()
                .filter_map(|at| timestamp(at))
                .filter(|at| *at <= now && start.is_none_or(|s| *at >= s))
                .min()
        })
        .flatten();
    let in_tool = eligible && !attempt.activity.in_flight_tools.is_empty();
    let long_tool = tool_started
        .is_some_and(|at| now.signed_duration_since(at).num_seconds() >= STALLED_AFTER_SECONDS);
    let stalled = eligible
        && !in_tool
        && quiet
            .is_some_and(|at| now.signed_duration_since(at).num_seconds() >= STALLED_AFTER_SECONDS);
    json!({"steps":steps,"quietSince":quiet.map(|at|at.to_rfc3339()),"stalled":stalled,"stallReason":if stalled {Some("noProgress")}else if long_tool {Some("longToolCall")}else{None},"toolCallStartedAt":tool_started.map(|at|at.to_rfc3339()),"stalledAfterSeconds":STALLED_AFTER_SECONDS})
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::asks::{
        model::{AttemptRecord, AttemptState, Hold, ProgressNote, StepProgress},
        store::{tests::new_ask, AskState},
    };
    fn at(s: &str) -> DateTime<Utc> {
        timestamp(s).unwrap()
    }
    pub(crate) fn fixture() -> (AskRecord, Value, DateTime<Utc>) {
        let now = at("2026-11-17T12:00:00Z");
        let mut state = AskState::default();
        let (mut ask, _) = state
            .create_ask(new_ask("board-progress", "work"), now)
            .unwrap();
        ask.execution = ExecutionState::Running;
        let mut a = AttemptRecord::submitting(1, "native".into(), 1, "2026-11-17T11:00:00Z".into());
        a.started_at = Some(a.submitted_at.clone());
        a.state = AttemptState::Running;
        a.turn_id = Some("turn".into());
        ask.attempts.push(a);
        let session = json!({"cutexSessionId":ask.target_cutex_session_id,"native":{"threadId":"thread"},"runtime":{"status":"online","runtimeGeneration":1,"activeTurnId":"turn"},"pauseReasons":[]});
        (ask, session, now)
    }
    #[test]
    fn board_output_deltas_are_activity_only_for_the_bound_turn() {
        let (ask, mut session, now) = fixture();
        session["activity"] =
            json!({"lastOutputAt":(now-chrono::Duration::seconds(1)).to_rfc3339()});
        let view = delegation(&ask, Some(&session), None, now);
        assert_eq!(view["stalled"], false);
        assert_eq!(
            view["quietSince"],
            (now - chrono::Duration::seconds(1)).to_rfc3339()
        );
        session["runtime"]["activeTurnId"] = json!("another-turn");
        assert_ne!(
            delegation(&ask, Some(&session), None, now)["quietSince"],
            view["quietSince"]
        );
    }
    #[test]
    fn board_progress_requires_structured_steps_or_this_attempts_plan() {
        let (mut ask, session, now) = fixture();
        ask.current_attempt_mut()
            .unwrap()
            .progress
            .push(ProgressNote {
                text: "finished 9/10, 90%".into(),
                at: now.to_rfc3339(),
                steps: None,
            });
        assert!(delegation(&ask, Some(&session), None, now)["steps"].is_null());
        let mut plan = json!({"threadId":"thread","turnId":"another-turn","updatedAt":now.to_rfc3339(),"plan":[{"step":"done","status":"completed"},{"step":"test","status":"inProgress"}]});
        assert!(delegation(&ask, Some(&session), Some(&plan), now)["steps"].is_null());
        plan["turnId"] = json!("turn");
        let value = delegation(&ask, Some(&session), Some(&plan), now);
        assert_eq!(value["steps"]["done"], 1);
        assert_eq!(value["steps"]["total"], 2);
        assert_eq!(value["steps"]["currentStep"], "test");
        plan["threadId"] = json!("foreign-thread");
        assert!(delegation(&ask, Some(&session), Some(&plan), now)["steps"].is_null());
        ask.current_attempt_mut()
            .unwrap()
            .progress
            .push(ProgressNote {
                text: "explicit progress".into(),
                at: now.to_rfc3339(),
                steps: Some(StepProgress {
                    done: 2,
                    total: 5,
                    current_step: Some("review".into()),
                }),
            });
        let value = delegation(&ask, Some(&session), Some(&plan), now);
        assert_eq!(value["steps"]["done"], 2);
        assert_eq!(value["steps"]["source"], "progressNote");
        assert!(StepProgress {
            done: 4,
            total: 3,
            current_step: None
        }
        .validate()
        .is_err());
        assert!(serde_json::from_value::<StepProgress>(json!({"done":-1,"total":3})).is_err());
    }
    #[test]
    fn board_stalled_threshold_waits_offline_stops_and_future_clock() {
        let (mut ask, mut session, now) = fixture();
        assert_eq!(delegation(&ask, Some(&session), None, now)["stalled"], true);
        ask.current_attempt_mut().unwrap().activity.last_activity_at =
            Some((now - chrono::Duration::seconds(899)).to_rfc3339());
        assert_eq!(
            delegation(&ask, Some(&session), None, now)["stalled"],
            false
        );
        ask.current_attempt_mut().unwrap().activity.last_activity_at =
            Some((now - chrono::Duration::seconds(900)).to_rfc3339());
        assert_eq!(delegation(&ask, Some(&session), None, now)["stalled"], true);
        ask.hold = Some(Hold::WaitingForInput);
        assert_eq!(
            delegation(&ask, Some(&session), None, now)["stalled"],
            false
        );
        ask.hold = None;
        ask.current_attempt_mut().unwrap().stop_requested = true;
        assert_eq!(
            delegation(&ask, Some(&session), None, now)["stalled"],
            false
        );
        ask.current_attempt_mut().unwrap().stop_requested = false;
        session["runtime"]["status"] = json!("offline");
        assert_eq!(
            delegation(&ask, Some(&session), None, now)["stalled"],
            false
        );
        session["runtime"]["status"] = json!("online");
        session["runtime"]["activeTurnId"] = json!("other-work");
        assert_eq!(
            delegation(&ask, Some(&session), None, now)["stalled"],
            false
        );
        session["runtime"]["activeTurnId"] = json!("turn");
        session["pauseReasons"] = json!(["ownerPaused"]);
        assert_eq!(
            delegation(&ask, Some(&session), None, now)["stalled"],
            false
        );
        session["pauseReasons"] = json!([]);
        let future = (now + chrono::Duration::hours(1)).to_rfc3339();
        let a = ask.current_attempt_mut().unwrap();
        a.started_at = Some(future.clone());
        a.submitted_at = future.clone();
        a.activity.last_activity_at = Some(future);
        let value = delegation(&ask, Some(&session), None, now);
        assert!(value["quietSince"].is_null());
        assert_eq!(value["stalled"], false);
    }
}
