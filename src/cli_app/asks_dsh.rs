//! Asks (委托) on DSH targets and requesters. ACP has no same-turn input,
//! so the DSH adapter (`components/dsh-runtime`) runs an asks input as its
//! own turn: `dsh/input/submit` with `admission: "exclusive_turn"` starts
//! that turn at once and returns its id, or refuses with the same
//! `externalInputError: "busy"` shape as cute-codex N1 and queues nothing.
//! `dsh/input/status` reports the turn and how it ended; the native log is
//! the proof that the input reached the model. Results for a busy requester
//! wait for its turn to end (no next-step admission), like cute-codex
//! without `soon`. An adapter without the capability (dsh-r1) keeps asks
//! held `runtimeUnsupported`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use cutex::app_server::client::AppServerHandle;
use cutex::asks::scheduler::{NativeMessageStatus, Submission, SubmitOutcome, TargetStatus, CUTEX_ASKS_SOURCE, EXCLUSIVE_TURN};
use cutex::session::model::CutexSessionRecord;

use super::app_server_runtime;
use super::app_server_user_input;

pub(crate) const ADMISSION: &str = "exclusive_turn";

fn handle(session: &str, generation: u64) -> anyhow::Result<AppServerHandle> {
    app_server_runtime::runtime_manager()
        .handle_for_generation(session, generation)
        .map_err(|error| anyhow::anyhow!("DSH runtime unavailable: {error:?}"))
}

/// The adapter advertises exclusive admission in `dsh/runtime/status`.
pub(crate) fn offers_exclusive(status: &Value) -> bool {
    status
        .pointer("/capabilities/inputAdmissions")
        .and_then(Value::as_array)
        .is_some_and(|values| values.iter().any(|value| value.as_str() == Some(ADMISSION)))
}

/// Cached per session and runtime generation (an adapter restart is a new
/// generation, so an upgraded adapter is asked again).
fn supports_exclusive(session: &str, generation: u64) -> bool {
    static CACHE: OnceLock<Mutex<HashMap<String, (u64, bool)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some((known, value)) = cache.lock().ok().and_then(|cache| cache.get(session).copied()) {
        if known == generation {
            return value;
        }
    }
    let Some(thread_id) = super::asks_runtime::record(session).and_then(|record| record.app_server_thread_id().map(str::to_string)) else {
        return false;
    };
    let Ok(status) = handle(session, generation)
        .and_then(|handle| Ok(handle.request("dsh/runtime/status", json!({"threadId": thread_id}))?))
    else {
        return false;
    };
    let value = offers_exclusive(&status);
    if let Ok(mut cache) = cache.lock() {
        cache.insert(session.to_string(), (generation, value));
    }
    value
}

pub(crate) fn target_status(session: &str, record: &CutexSessionRecord) -> TargetStatus {
    let status = app_server_runtime::runtime_manager().status(session).ok().flatten();
    let online = status
        .as_ref()
        .is_some_and(|status| status.connected && status.runtime_generation == record.runtime_generation);
    if !online {
        return TargetStatus {
            runtime_generation: record.runtime_generation,
            reconnectable: cutex::asks::wake::runtime_binding_live(record),
            ..TargetStatus::default()
        };
    }
    TargetStatus {
        online,
        runtime_generation: record.runtime_generation,
        active_turn_id: status.and_then(|status| status.active_turn_id),
        admission: None,
        plan_mode: false,
        profile_switch_pending: false,
        exclusive_admission: supports_exclusive(session, record.runtime_generation),
        blocked: None,
        reconnectable: false,
        receiver_limit: cutex::app_server::external_input::size::ReceiverLimit::RawText(
            cutex::app_server::external_input::size::TRANSPORT_TEXT_MAX as u64,
        ),
    }
}

/// `dsh/input/submit` params (deterministic: a retry with the same message
/// id carries the same semantic content).
pub(crate) fn submit_params(thread_id: &str, submission: &Submission) -> Value {
    let source = match &submission.source_agent {
        Some(agent) => json!({"kind": "agent", "id": agent}),
        None => json!({"kind": "service", "id": CUTEX_ASKS_SOURCE}),
    };
    json!({
        "threadId": thread_id,
        "messageId": submission.message_id,
        "source": source,
        "eventType": submission.event_type,
        "text": submission.text,
        "admission": ADMISSION,
    })
}

pub(crate) fn submit(session: &str, generation: u64, submission: &Submission) -> SubmitOutcome {
    let Some(record) = super::asks_runtime::record(session) else {
        return SubmitOutcome::Unsupported("no such session".into());
    };
    let Some(thread_id) = record.app_server_thread_id().map(str::to_string) else {
        return SubmitOutcome::Unsupported("DSH thread missing".into());
    };
    if cutex::asks::returns::is_next_step_event(&submission.message_id) {
        return SubmitOutcome::Unsupported("DSH has no next-step admission".into());
    }
    // Serialize with owner submit / flush / stop.
    let lock = match app_server_user_input::session_input_lock(session) {
        Ok(lock) => lock,
        Err(error) => return SubmitOutcome::Unsupported(format!("{error:#}")),
    };
    let Ok(_guard) = lock.lock() else {
        return SubmitOutcome::Unsupported("session input lock poisoned".into());
    };
    // r266: a project or agent pause may have committed after the scheduler
    // persisted this admission and released the store (or while this submit
    // waited for the lane). Re-check inside the lane, like native and Claude;
    // a briefly contended authority fence is retried, not refused at once.
    if let Some(outcome) = super::asks_runtime::pause_admission_outcome_in_lane(session) {
        return outcome;
    }
    if let Ok(Some(status)) = app_server_runtime::runtime_manager().status(session) {
        if status.active_turn_id.is_some() {
            return SubmitOutcome::Busy { reason: "active_turn".into() };
        }
    }
    if !supports_exclusive(session, generation) {
        return SubmitOutcome::Unsupported(EXCLUSIVE_TURN.into());
    }
    let handle = match handle(session, generation) {
        Ok(handle) => handle,
        Err(error) => return SubmitOutcome::Unsupported(format!("{error:#}")),
    };
    super::asks_claude::submit_outcome(handle.request("dsh/input/submit", submit_params(&thread_id, submission)))
}

pub(crate) fn message_status(session: &str, generation: u64, submission: &Submission) -> NativeMessageStatus {
    let Some(thread_id) = super::asks_runtime::record(session).and_then(|record| record.app_server_thread_id().map(str::to_string))
    else {
        return NativeMessageStatus::Unavailable("DSH thread missing".into());
    };
    let handle = match handle(session, generation) {
        Ok(handle) => handle,
        Err(error) => return NativeMessageStatus::Unavailable(format!("{error:#}")),
    };
    super::asks_claude::status_outcome(handle.request(
        "dsh/input/status",
        json!({"threadId": thread_id, "messageId": submission.message_id}),
    ))
}

/// r266 exact stop (project pause interrupt, StopAttempt) of a DSH or Kimi
/// turn. ACP cancel ends the turn and the adapter has no input gate to keep
/// closed, so the exact form is the same `turn/interrupt`, fenced three ways:
/// the runtime manager must show this generation connected with exactly
/// `turn_id` active; the handle is bound to that generation (it never
/// reconnects or launches a replacement occurrence); and the adapter itself
/// refuses a `turnId` that is not its active turn. One RPC, never retried:
/// an error after sending leaves the outcome unknown to the caller, which
/// never replays it.
pub(crate) fn interrupt_exact(session: &str, generation: u64, turn_id: &str) -> anyhow::Result<()> {
    // The caller (asks_runtime) already holds this lane; same-thread
    // nesting shares it, so restart/owner stop stay serialized.
    let lane = app_server_user_input::session_input_lock(session)?;
    let _operation = lane.lock().map_err(|_| anyhow::anyhow!("session input lock poisoned"))?;
    let record = super::asks_runtime::record(session).ok_or_else(|| anyhow::anyhow!("no such session"))?;
    anyhow::ensure!(record.runtime_generation == generation, "runtime_generation_changed");
    let thread_id = record.app_server_thread_id().ok_or_else(|| anyhow::anyhow!("DSH thread missing"))?.to_string();
    let status = app_server_runtime::runtime_manager().status(session)?
        .ok_or_else(|| anyhow::anyhow!("runtime_occurrence_changed"))?;
    anyhow::ensure!(status.connected && status.runtime_generation == generation
        && status.active_turn_id.as_deref() == Some(turn_id), "runtime_occurrence_changed");
    let original = handle(session, generation)?;
    interrupt_exact_bound(&thread_id, turn_id, |method, params| Ok(original.request(method, params)?))
}

/// The single turn-scoped ACP cancel (`FnOnce`: no second attempt).
fn interrupt_exact_bound(thread_id: &str, turn_id: &str,
    request: impl FnOnce(&str, Value) -> anyhow::Result<Value>) -> anyhow::Result<()> {
    request("turn/interrupt", json!({"threadId": thread_id, "turnId": turn_id}))?;
    Ok(())
}

/// ACP cancel ends the turn; the adapter has no input gate to keep closed.
pub(crate) fn interrupt(session: &str, turn_id: &str) -> anyhow::Result<()> {
    let record = super::asks_runtime::record(session).ok_or_else(|| anyhow::anyhow!("no such session"))?;
    let thread_id = record.app_server_thread_id().ok_or_else(|| anyhow::anyhow!("DSH thread missing"))?.to_string();
    handle(session, record.runtime_generation)?
        .request("turn/interrupt", json!({"threadId": thread_id, "turnId": turn_id}))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_capability_and_params_are_deterministic() {
        assert!(offers_exclusive(&json!({"capabilities": {"inputAdmissions": ["exclusive_turn"]}})));
        assert!(!offers_exclusive(&json!({"capabilities": {"resume": true}})));
        let submission = Submission {
            message_id: "ask-1:attempt-1".into(), source_agent: Some("cutex.requester".into()), event_type: "ASK",
            text: "do it".into(), asks: vec!["ask-1".into()], attempt: Some(1), view: Some(json!({"schema": "x"})),
        };
        let params = submit_params("thread-1", &submission);
        assert_eq!(params, json!({"threadId": "thread-1", "messageId": "ask-1:attempt-1",
            "source": {"kind": "agent", "id": "cutex.requester"}, "eventType": "ASK", "text": "do it", "admission": "exclusive_turn"}));
        let owner = Submission { source_agent: None, ..submission };
        assert_eq!(submit_params("thread-1", &owner)["source"], json!({"kind": "service", "id": CUTEX_ASKS_SOURCE}));
    }

    // ---- r266 exact stop (B2) -------------------------------------------

    fn acp_record(id: &str, flavor: &str) -> CutexSessionRecord {
        let mut record = CutexSessionRecord::new(id.into(), None, cutex::platform::host::current_host_name(), "/tmp".into(), None).unwrap();
        record.runtime_backend = if flavor == "kimi" {
            cutex::session::model::CutexSessionRuntimeBackend::Kimi
        } else {
            cutex::session::model::CutexSessionRuntimeBackend::Dsh
        };
        record.runtime_generation = 7;
        record.dsh_session = Some(serde_json::from_value(json!({
            "flavor": flavor, "thread_id": format!("thread-{flavor}"), "state_dir": "/s", "node": "/n", "adapter": "/a",
            "dsh_cli": "/d", "patch": "/s/profile.patch.yml", "install_root": "/i", "native_session_id": null, "spike": false,
        })).unwrap());
        record
    }

    fn save(records: &[&CutexSessionRecord]) {
        let mut store = cutex::session::model::CutexSessionStore::default();
        for record in records {
            store.sessions.insert(record.cutex_session_id.clone(), (*record).clone());
        }
        cutex::session::store::save_cutex_session_store(&store).unwrap();
    }

    /// QA dispatch-proof (r265b-kimi B2): the r266 exact stop of a DSH/Kimi
    /// turn went to the cute-codex ExternalInput client ("ingress explicit
    /// launch absent", 0 ACP cancels). It must take the ACP route, like the
    /// legacy keep-gate interrupt does.
    #[test]
    fn exact_stop_of_dsh_and_kimi_dispatches_to_acp_not_external_input() {
        use super::super::asks_runtime::{exact_interrupt_route_for_tests as route, ExactInterruptRoute};
        for flavor in ["dsh", "kimi"] {
            let record = acp_record(&format!("cutex.exact-{flavor}"), flavor);
            assert!(record.explicit_launch.is_none(), "ACP records have no ExternalInput launch");
            assert_eq!(route(&record), ExactInterruptRoute::Acp, "{flavor}");
        }
        let native = CutexSessionRecord::new("cutex.exact-native".into(), None, "h".into(), "/tmp".into(), None).unwrap();
        assert_eq!(route(&native), ExactInterruptRoute::Native);
    }

    #[test]
    fn exact_acp_stop_is_one_turn_scoped_cancel_and_never_retries() {
        let calls = std::cell::RefCell::new(Vec::new());
        interrupt_exact_bound("thread-1", "turn-9", |method, params| {
            calls.borrow_mut().push((method.to_string(), params));
            Ok(json!({}))
        }).unwrap();
        assert_eq!(*calls.borrow(), vec![("turn/interrupt".to_string(), json!({"threadId": "thread-1", "turnId": "turn-9"}))]);
        // An error after sending (adapter: "turn is not active", or a broken
        // connection) is returned once; the caller records it as unknown.
        let attempts = std::cell::Cell::new(0);
        let error = interrupt_exact_bound("thread-1", "turn-9", |_, _| {
            attempts.set(attempts.get() + 1);
            anyhow::bail!("turn is not active")
        }).unwrap_err();
        assert_eq!(attempts.get(), 1);
        assert!(!format!("{error:#}").contains("exact_interrupt_unsupported"), "must not degrade to unsupported");
    }

    /// The production adapter entry keeps r266's fences for ACP records: a
    /// changed generation or a missing/replaced occurrence fails before any
    /// transport lookup, and never reports "unsupported".
    #[test]
    fn exact_acp_stop_keeps_generation_and_occurrence_fences() {
        use cutex::asks::scheduler::AskRuntime;
        let _home = crate::cli_app::test_home::IsolatedTestHome::new_private("acp-exact-stop-fence").unwrap();
        let dsh = acp_record("cutex.acp-exact-dsh", "dsh");
        let kimi = acp_record("cutex.acp-exact-kimi", "kimi");
        save(&[&dsh, &kimi]);
        let runtime = super::super::asks_runtime::ManagementAskRuntime::default();
        for id in [&dsh.cutex_session_id, &kimi.cutex_session_id] {
            let stale = runtime.interrupt_exact(id, 6, "turn-1").unwrap_err().to_string();
            assert!(stale.contains("runtime_generation_changed"), "{stale}");
            // No connected occurrence of generation 7 in this process.
            let gone = format!("{:#}", runtime.interrupt_exact(id, 7, "turn-1").unwrap_err());
            assert!(gone.contains("runtime_occurrence_changed"), "{gone}");
            assert!(!gone.contains("explicit launch") && !gone.contains("exact_interrupt_unsupported"), "{gone}");
        }
    }

    // ---- r266 pause admission inside the lane (B3) ----------------------

    fn submission(id: &str) -> Submission {
        Submission {
            message_id: format!("{id}:attempt-1"), source_agent: Some("cutex.requester".into()), event_type: "ASK",
            text: "do it".into(), asks: vec![id.into()], attempt: Some(1), view: None,
        }
    }

    fn project_pause(session: &str) -> (String, u64) {
        use cutex::asks::project_control as p;
        let store = cutex::asks::store::AskStore::open_default().unwrap();
        store.with_state(|state| {
            p::pause(state, &project_ctx(session), &p::PauseRequest { action_id: "pause".into(), expected_epoch: 0, mode: p::Mode::AfterTurn, scope: p::Scope::Members },
                &p::Membership { revision: 0, members: [session.to_string()].into(), excluded: Default::default() }, "test-host", chrono::Utc::now()).unwrap();
            let control = &state.project_control.projects[&format!("project-{session}")];
            (control.pause_id.clone().unwrap(), control.epoch)
        }).unwrap()
    }

    fn project_ctx(session: &str) -> cutex::asks::routing_policy::RoutingContext {
        use cutex::asks::routing_policy::{RoutingActor, RoutingContext};
        RoutingContext { project_id: format!("project-{session}"), authority_epoch: 0, current_director: None, actor: RoutingActor::Owner, operator_may_route: false }
    }

    fn project_resume(session: &str, pause_id: String, epoch: u64) {
        use cutex::asks::project_control as p;
        let store = cutex::asks::store::AskStore::open_default().unwrap();
        store.with_state(|state| {
            p::resume(state, &project_ctx(session), &p::ResumeRequest { action_id: "resume".into(), pause_id, expected_epoch: epoch }, "test-host", chrono::Utc::now()).unwrap();
        }).unwrap();
    }

    fn submit_via_runtime(session: &str) -> SubmitOutcome {
        use cutex::asks::scheduler::AskRuntime;
        super::super::asks_runtime::ManagementAskRuntime::default().submit_exclusive(session, 7, &submission("ask-acp"))
    }

    /// QA barrier (r265b-kimi B3): the scheduler persisted the admission
    /// lease, released store/authority, and a project pause committed
    /// before the ACP submit acquired the session lane. The submit must
    /// re-check the pause inside the lane and hand back a retryable Busy,
    /// never reach the runtime. After resume the same submit goes on (here:
    /// to the runtime lookup, as no adapter runs in this test).
    #[test]
    fn acp_submit_rechecks_project_pause_inside_the_lane_then_resumes() {
        let _home = crate::cli_app::test_home::IsolatedTestHome::new_private("acp-submit-pause-barrier").unwrap();
        let dsh = acp_record("cutex.acp-pause-dsh", "dsh");
        let kimi = acp_record("cutex.acp-pause-kimi", "kimi");
        save(&[&dsh, &kimi]);
        for record in [&dsh, &kimi] {
            let session = record.cutex_session_id.clone();
            let store = cutex::asks::store::AskStore::open_default().unwrap();
            assert_eq!(cutex::asks::pause_admission::gate(&store, &session).unwrap(), cutex::asks::pause_admission::Gate::Open);
            // Barrier: hold the session lane so the submit is past admission
            // but before lane acquisition, then pause.
            let lane = app_server_user_input::session_input_lock(&session).unwrap();
            let held = lane.lock().unwrap();
            let worker = {
                let session = session.clone();
                std::thread::spawn(move || submit_via_runtime(&session))
            };
            let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while lane.snapshot().unwrap().waiting == 0 && std::time::Instant::now() < until {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert_eq!(lane.snapshot().unwrap().waiting, 1, "submit did not reach the lane");
            let (pause_id, epoch) = project_pause(&session);
            drop(held);
            assert_eq!(worker.join().unwrap(), SubmitOutcome::Busy { reason: "agent_paused".into() }, "{session}");
            // Retry while still paused: held again, no runtime effect.
            assert_eq!(submit_via_runtime(&session), SubmitOutcome::Busy { reason: "agent_paused".into() });
            project_resume(&session, pause_id, epoch);
            // Resumed: admission is open again and the submit proceeds to the
            // runtime (none here: refused as unavailable, never as paused).
            let resumed = submit_via_runtime(&session);
            assert!(!matches!(resumed, SubmitOutcome::Busy { .. }), "{resumed:?}");
            assert!(matches!(resumed, SubmitOutcome::Unsupported(_)), "{resumed:?}");
        }
    }

    /// The scheduler's lease write wakes the admission thread, whose short
    /// authority-fence holds used to collide with this one-shot check on every
    /// retry (E2E: 188 `authority_busy` refusals, the ask starved). A fence
    /// released within the bounded window no longer refuses the submit.
    #[test]
    fn acp_submit_rides_out_a_brief_authority_fence_hold() {
        let _home = crate::cli_app::test_home::IsolatedTestHome::new_private("acp-submit-brief-fence").unwrap();
        let dsh = acp_record("cutex.acp-brief-fence-dsh", "dsh");
        save(&[&dsh]);
        let session = dsh.cutex_session_id.clone();
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            cutex::asks::pause_admission::try_with_authority(|_| {
                held_tx.send(()).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(150));
                Ok(())
            }).unwrap()
        });
        held_rx.recv().unwrap();
        let outcome = submit_via_runtime(&session);
        assert_eq!(holder.join().unwrap(), cutex::asks::pause_admission::Admission::Ready(()));
        // Past the gate: refused only because no adapter runs in this test.
        assert!(matches!(outcome, SubmitOutcome::Unsupported(_)), "{outcome:?}");
    }

    /// An individual (whole-agent) pause is held at the same in-lane check,
    /// and lifecycle contention defers instead of submitting.
    #[test]
    fn acp_submit_holds_agent_pause_and_defers_on_lifecycle_contention() {
        let _home = crate::cli_app::test_home::IsolatedTestHome::new_private("acp-submit-agent-pause").unwrap();
        let kimi = acp_record("cutex.acp-agent-pause-kimi", "kimi");
        save(&[&kimi]);
        let session = kimi.cutex_session_id.clone();
        let store = cutex::asks::store::AskStore::open_default().unwrap();
        let held = cutex::asks::pause_admission::try_with_authority(|_| Ok(submit_via_runtime(&session))).unwrap();
        assert_eq!(held, cutex::asks::pause_admission::Admission::Ready(SubmitOutcome::Busy { reason: "authority_busy".into() }));
        store.with_state(|state| {
            state.presence.entry(session.clone()).or_default().busy = false;
            cutex::asks::agent_pause::pause(state, &session, "owner", None, chrono::Utc::now());
        }).unwrap();
        assert_eq!(submit_via_runtime(&session), SubmitOutcome::Busy { reason: "agent_paused".into() });
        store.with_state(|state| { cutex::asks::agent_pause::resume(state, &session); }).unwrap();
        assert!(matches!(submit_via_runtime(&session), SubmitOutcome::Unsupported(_)));
    }
}
