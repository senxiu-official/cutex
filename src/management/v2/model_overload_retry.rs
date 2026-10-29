//! Bounded fresh continuation after a typed terminal model-overload failure.
//! This journal never contains/replays the original owner input or tool call.
use crate::http::server::{write_json_response, SimpleHttpRequest};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, net::TcpStream, path::PathBuf, sync::Mutex};

pub const FEATURE: &str = "model-overload-auto-retry";
pub const PATH: &str = "/v2/model-overload-retry";
pub const CHANGED_EVENT: &str = "cutex/modelOverloadRetry/changed";
pub const CONTINUE_TEXT: &str = "Continue.";
pub const DELAYS: [i64; 3] = [15, 45, 120];

pub fn overloaded(turn: &Value) -> bool {
    turn["status"] == "failed"
        && match &turn["error"]["codexErrorInfo"] {
            Value::String(kind) => kind == "serverOverloaded",
            Value::Object(kind) => kind.len() == 1 && kind.contains_key("serverOverloaded"),
            _ => false,
        }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    Pending,
    Firing,
    Running,
    Uncertain,
    Binding,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub plan_id: String,
    pub thread_id: String,
    pub runtime_generation: u64,
    pub turn_id: String,
    pub attempt: u32,
    pub at: String,
    pub scheduled_at: String,
    pub phase: Phase,
    pub message_id: String,
    pub failed_turn: Value,
    pub lease: Option<crate::asks::model::AdmissionLease>,
    #[serde(default)]
    pub early_terminal: Option<Value>,
    #[serde(default)]
    pub early_message: Option<Value>,
    #[serde(default)]
    pub receipt_checks: u32,
    #[serde(default)]
    pub receipt_check_at: Option<String>,
    #[serde(default)]
    pub binding_turn_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settlement {
    pub plan: Plan,
    pub reason: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Entry {
    pub revision: u64,
    pub plan: Option<Plan>,
    pub last: Option<Value>,
    pub seen: Vec<String>,
    /// An unresolved send cancelled by the owner must not resurrect when its
    /// previously unknown turn ends later. A later owner input clears this.
    pub blocked_generation: Option<u64>,
    pub settlement: Option<Settlement>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct State {
    pub version: u32,
    pub sessions: BTreeMap<String, Entry>,
}

fn stamp(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

impl State {
    pub fn view(&self, session: &str) -> Value {
        let entry = self.sessions.get(session);
        let pending = entry
            .and_then(|e| e.plan.as_ref())
            .filter(|p| p.phase == Phase::Pending)
            .map(|p| {
                json!({
                    "at":p.at, "attempt":p.attempt, "maxAttempts":3, "turnId":p.turn_id,
                    "scheduledAt":p.scheduled_at, "planId":p.plan_id,
                })
            });
        json!({"cutexSessionId":session,"revision":entry.map_or(0, |e|e.revision),
            "pending":pending,"last":entry.and_then(|e|e.last.as_ref())})
    }

    /// Called before the Ask observer. A duplicate terminal never extends a timer.
    pub fn terminal(
        &mut self,
        session: &str,
        thread: &str,
        generation: u64,
        turn: &Value,
        lease: Option<crate::asks::model::AdmissionLease>,
        now: DateTime<Utc>,
    ) -> bool {
        let Some(id) = turn
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            return false;
        };
        if !overloaded(turn) && !self.sessions.get(session).is_some_and(|e| e.plan.is_some()) {
            return false;
        }
        let entry = self.sessions.entry(session.into()).or_default();
        if entry.settlement.is_some() {
            return false;
        }
        if entry.blocked_generation == Some(generation) {
            return false;
        }
        if entry.seen.iter().any(|old| old == id) {
            return false;
        }
        if let Some(plan) = entry.plan.as_mut() {
            if plan.runtime_generation != generation || plan.thread_id != thread {
                return false;
            }
            if matches!(
                plan.phase,
                Phase::Firing | Phase::Uncertain | Phase::Binding
            ) && plan.turn_id != id
            {
                plan.early_terminal = Some(turn.clone());
                return true;
            }
            if plan.turn_id != id {
                return false;
            }
            if plan.phase != Phase::Running {
                return false;
            }
        } else if lease
            .as_ref()
            .is_some_and(|l| l.turn_id.as_deref() != Some(id) || l.runtime_generation != generation)
        {
            return false;
        }
        entry.seen.push(id.into());
        if entry.seen.len() > 64 {
            entry.seen.remove(0);
        }
        let previous = entry.plan.take();
        let attempt = previous.as_ref().map_or(1, |p| p.attempt + 1);
        if !overloaded(turn) || attempt > 3 {
            entry.last = Some(
                json!({"at":stamp(now),"result":if overloaded(turn){"exhausted"}else{"cancelled"},
                "reason":if overloaded(turn){"maxAttempts"}else{"turnEnded"},"attempt":attempt.saturating_sub(1)}),
            );
            return previous.is_some();
        }
        let plan_id = previous
            .as_ref()
            .map(|p| p.plan_id.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        entry.plan = Some(Plan {
            message_id: format!("cutex-model-overload-{plan_id}-{attempt}"),
            plan_id,
            thread_id: thread.into(),
            runtime_generation: generation,
            turn_id: id.into(),
            attempt,
            at: stamp(now + chrono::Duration::seconds(DELAYS[(attempt - 1) as usize])),
            scheduled_at: stamp(now),
            phase: Phase::Pending,
            failed_turn: turn.clone(),
            lease: previous.and_then(|p| p.lease).or(lease),
            early_terminal: None,
            early_message: None,
            receipt_checks: 0,
            receipt_check_at: None,
            binding_turn_id: None,
        });
        true
    }

    pub fn cancel(&mut self, session: &str, reason: &str, now: DateTime<Utc>) -> Option<Plan> {
        let entry = self.sessions.get_mut(session)?;
        if let Some(pending) = &entry.settlement {
            return Some(pending.plan.clone());
        }
        let plan = entry.plan.take()?;
        if !entry.seen.contains(&plan.turn_id) {
            entry.seen.push(plan.turn_id.clone());
        }
        if entry.seen.len() > 64 {
            entry.seen.remove(0);
        }
        if matches!(plan.phase, Phase::Uncertain | Phase::Binding) {
            entry.blocked_generation = Some(plan.runtime_generation);
        }
        entry.last = Some(
            json!({"at":stamp(now),"result":cancellation_outcome(reason),"reason":reason,"attempt":plan.attempt}),
        );
        entry.settlement = Some(Settlement {
            plan: plan.clone(),
            reason: reason.into(),
        });
        Some(plan)
    }

    pub fn claim(&mut self, session: &str, plan_id: &str, now: DateTime<Utc>) -> Option<Plan> {
        let plan = self.sessions.get_mut(session)?.plan.as_mut()?;
        if plan.plan_id != plan_id
            || plan.phase != Phase::Pending
            || DateTime::parse_from_rfc3339(&plan.at)
                .ok()?
                .with_timezone(&Utc)
                > now
        {
            return None;
        }
        plan.phase = Phase::Firing;
        Some(plan.clone())
    }

    pub fn holds(&self, session: &str, turn: Option<&str>) -> bool {
        self.sessions.get(session).is_some_and(|e| {
            let p = e
                .plan
                .as_ref()
                .or_else(|| e.settlement.as_ref().map(|s| &s.plan));
            p.is_some_and(|p| {
                p.phase != Phase::Running
                    && turn.is_none_or(|id| {
                        p.turn_id == id || p.binding_turn_id.as_deref() == Some(id)
                    })
            })
        })
    }
}

static LOCK: Mutex<()> = Mutex::new(());
static DISABLED_WITHOUT_STORE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
pub struct Store {
    path: PathBuf,
    disabled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
impl Store {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        // Registry lock only protects pointers: no filesystem/native IO under it.
        // Reopening the same journal cannot clear this process-lifetime latch.
        static LATCHES: std::sync::OnceLock<
            Mutex<BTreeMap<PathBuf, std::sync::Arc<std::sync::atomic::AtomicBool>>>,
        > = std::sync::OnceLock::new();
        let path = path.into();
        let disabled = LATCHES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(path.clone())
            .or_default()
            .clone();
        Self { path, disabled }
    }
    pub fn open_default() -> anyhow::Result<Self> {
        Ok(Self::at(
            crate::config::paths::runtime_dir()?.join("model-overload-retry/state-v1.json"),
        ))
    }
    fn read(&self) -> anyhow::Result<State> {
        match std::fs::read(&self.path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(e.into()),
        }
    }
    // Lock a stable sidecar, not the atomically replaced JSON inode. Held over
    // the entire read/change/write by every reader and writer in every process.
    fn locks(
        &self,
        immediate: bool,
    ) -> anyhow::Result<(std::sync::MutexGuard<'static, ()>, std::fs::File)> {
        use fs2::FileExt;
        // A retry worker may hold the native session lane while reading this
        // journal. Bound every journal lock wait, not just the safety caller.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let local = loop {
            match LOCK.try_lock() {
                Ok(guard) => break guard,
                Err(std::sync::TryLockError::Poisoned(e)) => break e.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => {
                    anyhow::ensure!(
                        !immediate && std::time::Instant::now() < deadline,
                        "retry journal process lock busy"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
        };
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.path.with_extension("lock"))?;
        loop {
            match FileExt::try_lock_exclusive(&file) {
                Ok(()) => break,
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
                {
                    anyhow::ensure!(
                        !immediate && std::time::Instant::now() < deadline,
                        "retry journal OS lock busy"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok((local, file))
    }
    pub fn automatic_enabled(&self) -> bool {
        !self.is_disabled() && self.snapshot().is_ok()
    }
    pub fn is_disabled(&self) -> bool {
        self.disabled.load(std::sync::atomic::Ordering::Acquire)
            || DISABLED_WITHOUT_STORE.load(std::sync::atomic::Ordering::Acquire)
            || self.path.with_extension("disabled").exists()
    }
    pub(crate) fn disable(&self) -> anyhow::Result<()> {
        self.disable_with(|| {
            let path = self.path.with_extension("disabled");
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&path)
                .map_err(|e| {
                    anyhow::anyhow!(
                        "process retry latch set; cannot create durable disable marker {}: {e}",
                        path.display()
                    )
                })?;
            file.sync_all().map_err(|e| {
                anyhow::anyhow!(
                    "process retry latch set; cannot sync durable disable marker {}: {e}",
                    path.display()
                )
            })
        })
    }
    fn disable_with(&self, persist: impl FnOnce() -> anyhow::Result<()>) -> anyhow::Result<()> {
        self.disabled
            .store(true, std::sync::atomic::Ordering::Release);
        // A journal fault must not prevent a safety operation. A sticky fence
        // survives storage recovery; successful persistence also fences restart.
        // The in-process latch has no reset API, even if marker persistence fails.
        persist()
    }
    pub fn snapshot(&self) -> anyhow::Result<State> {
        let (_guard, _file) = self.locks(false)?;
        self.read()
    }
    pub fn update<T>(&self, change: impl FnOnce(&mut State) -> T) -> anyhow::Result<T> {
        let (_guard, _file) = self.locks(false)?;
        self.update_locked(change)
    }
    fn try_update<T>(&self, change: impl FnOnce(&mut State) -> T) -> anyhow::Result<T> {
        let (_guard, _file) = self.locks(true)?;
        self.update_locked(change)
    }
    fn update_locked<T>(&self, change: impl FnOnce(&mut State) -> T) -> anyhow::Result<T> {
        let mut state = self.read()?;
        let before = state.clone();
        let result = change(&mut state);
        if state != before {
            for (session, entry) in &mut state.sessions {
                if before.sessions.get(session) != Some(entry) {
                    entry.revision = entry.revision.saturating_add(1);
                }
            }
            state.version = 1;
            if let Some(parent) = self.path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            crate::config::atomic::write_private_pretty_json_atomic(
                &self.path,
                &state,
                "model overload retry",
            )?;
        }
        Ok(result)
    }
}

pub fn holds_default(session: &str, turn: Option<&str>) -> bool {
    // A damaged shared journal must not hold every unrelated delegation.
    Store::open_default()
        .and_then(|s| s.snapshot())
        .map(|s| s.holds(session, turn))
        .unwrap_or(false)
}

pub fn cancellation_outcome(reason: &str) -> &'static str {
    match reason {
        "receiptUnresolved" | "resumeUnavailable" => "failed",
        "owner"
        | "ownerMessage"
        | "ownerStop"
        | "paused"
        | "stopped"
        | "runtimeOffline"
        | "runtimeRestarted"
        | "archived"
        | "managementRestarted" => "cancelled",
        _ => "skipped",
    }
}

pub fn publish(session: &str, reason: &str) {
    let Ok(state) = Store::open_default().and_then(|s| s.snapshot()) else {
        return;
    };
    let Ok(events) = super::repository::management_v2_repository() else {
        return;
    };
    let _ = super::server::append_cutex_event(
        events,
        super::services::HOST_EVENT_SESSION_ID,
        super::model::EventCorrelation::default(),
        CHANGED_EVENT,
        json!({"cutexSessionId":session,
            "hostId":crate::platform::host::current_host_name(),"reason":reason,"modelOverloadRetry":state.view(session)}),
    );
}

/// Firing has not submitted. Uncertain may have executed and must never be
/// replayed. Running belongs to a receipt-bound turn and normal observation.
pub fn settle_cancelled(
    asks: &crate::asks::store::AskStore,
    session: &str,
    plan: &Plan,
    reason: &str,
) -> anyhow::Result<()> {
    use crate::asks::model::{InterruptReason, LeaseHolder};
    if plan.phase == Phase::Running {
        return Ok(());
    }
    let state = asks.snapshot()?;
    let Some(lease) = state.leases.get(session) else {
        return Ok(());
    };
    if plan.lease.as_ref().is_none_or(|expected| {
        expected.holder != lease.holder || expected.native_message_id != lease.native_message_id
    }) || lease.runtime_generation != plan.runtime_generation
        || !lease
            .turn_id
            .as_deref()
            .is_some_and(|id| id == plan.turn_id || plan.binding_turn_id.as_deref() == Some(id))
    {
        return Ok(());
    }
    let uncertain = matches!(plan.phase, Phase::Uncertain | Phase::Binding);
    let stopped = matches!(reason, "ownerStop" | "paused" | "stopped");
    if let LeaseHolder::Ask { ask_id, attempt } = &lease.holder {
        if uncertain || stopped {
            return crate::asks::observe::finalize_fenced(
                asks,
                session,
                ask_id,
                *attempt,
                &json!({"id":lease.turn_id,"status":"interrupted"}),
                Some(if uncertain {
                    InterruptReason::OutcomeUnknown
                } else {
                    InterruptReason::OwnerStopped
                }),
                Utc::now(),
                lease,
            );
        }
    }
    if let LeaseHolder::Ask { ask_id, attempt } = &lease.holder {
        return crate::asks::observe::finalize_fenced(
            asks,
            session,
            ask_id,
            *attempt,
            &plan.failed_turn,
            Some(InterruptReason::RuntimeRestart),
            Utc::now(),
            lease,
        );
    }
    if let LeaseHolder::Event { event_id } = &lease.holder {
        asks.with_state(|state| {
            if state.leases.get(session) == Some(lease) {
                crate::asks::delivery::acknowledge_event(
                    state,
                    event_id,
                    lease.turn_id.as_deref().unwrap_or(&plan.turn_id),
                    Utc::now(),
                );
                state.leases.remove(session);
            }
        })?;
    }
    if matches!(lease.holder, LeaseHolder::Owner) {
        asks.with_state(|state| {
            if state.leases.get(session) == Some(lease) {
                state.leases.remove(session);
            }
        })?;
    }
    Ok(())
}

pub fn cancel_with(
    store: &Store,
    asks: &crate::asks::store::AskStore,
    session: &str,
    reason: &str,
) -> anyhow::Result<bool> {
    let lane = super::session_coordination::session_lane(session)?;
    let _reservation = lane.lock()?;
    let plan = store.update(|s| {
        if reason == "ownerMessage" {
            if let Some(e) = s.sessions.get_mut(session) {
                e.blocked_generation = None;
            }
        }
        s.cancel(session, reason, Utc::now())
    })?;
    if plan.is_some() {
        recover_settlement(store, asks, session)?;
        return Ok(true);
    }
    Ok(false)
}

/// Replay the durable settlement until AskStore has committed; only then clear
/// the journal record. A crash on either side is safe because finalize is fenced
/// and idempotent. Do not replace the original reason on recovery.
pub fn recover_settlement(
    store: &Store,
    asks: &crate::asks::store::AskStore,
    session: &str,
) -> anyhow::Result<()> {
    let Some(pending) = store
        .snapshot()?
        .sessions
        .get(session)
        .and_then(|e| e.settlement.clone())
    else {
        return Ok(());
    };
    settle_cancelled(asks, session, &pending.plan, &pending.reason)?;
    store.update(|s| {
        if let Some(e) = s.sessions.get_mut(session) {
            if e.settlement.as_ref() == Some(&pending) {
                e.settlement = None;
            }
        }
    })
}

pub fn cancel_default(session: &str, reason: &str) -> anyhow::Result<bool> {
    let changed = cancel_with(
        &Store::open_default()?,
        &crate::asks::store::AskStore::open_default()?,
        session,
        reason,
    )?;
    if changed {
        publish(session, cancellation_outcome(reason));
    }
    Ok(changed)
}

/// Compare/remove under the journal lock, inside the session lane.
pub fn compare_cancel(
    store: &Store,
    asks: &crate::asks::store::AskStore,
    session: &str,
    plan_id: &str,
    revision: u64,
) -> anyhow::Result<bool> {
    let lane = super::session_coordination::session_lane(session)?;
    let _reservation = lane.lock()?;
    let plan = store.update(|s| {
        let e = s.sessions.get(session)?;
        let p = e.plan.as_ref()?;
        if e.revision != revision || p.plan_id != plan_id || p.phase != Phase::Pending {
            return None;
        }
        s.cancel(session, "owner", Utc::now())
    })?;
    if plan.is_some() {
        recover_settlement(store, asks, session)?;
        return Ok(true);
    }
    Ok(false)
}

pub fn dispatch_after_cancel<T>(
    session: &str,
    method: &str,
    dispatch: impl FnOnce() -> Result<T, super::user_input::UserInputExecutionError>,
) -> Result<T, super::user_input::UserInputExecutionError> {
    dispatch_after_cancel_with_reservation(session, method, None, dispatch)
}

/// HTTP already owns the session lane through durable request settlement.
/// Reuse that capability only after checking the actual lane and owning thread;
/// callers without one keep the ordinary nonblocking safety admission path.
pub(crate) fn dispatch_after_cancel_with_reservation<T>(
    session: &str,
    method: &str,
    reservation: Option<&super::session_coordination::OperationReservation<'_>>,
    dispatch: impl FnOnce() -> Result<T, super::user_input::UserInputExecutionError>,
) -> Result<T, super::user_input::UserInputExecutionError> {
    if let Some(reservation) = reservation {
        let valid = super::session_coordination::session_lane(session)
            .and_then(|lane| Ok(reservation.is_current_for(&lane)?));
        if !matches!(valid, Ok(true)) {
            return Err(super::server::persistence_user_input_error(anyhow::anyhow!(
                "retry cancellation requires the current session operation capability"
            )));
        }
    }
    let Some(reason) = cancel_method(method) else {
        return dispatch();
    };
    let store = Store::open_default();
    if safety_reason(reason) {
        return match store {
            Ok(store) => dispatch_after_safety_cancel_inner(&store, session, reason, reservation, dispatch),
            Err(error) => dispatch_after_cancel_result(None, reason, Err(error), dispatch),
        };
    }
    let result = cancel_default(session, reason);
    dispatch_after_cancel_result(store.as_ref().ok(), reason, result, dispatch)
}

fn safety_reason(reason: &str) -> bool {
    matches!(
        reason,
        "ownerStop" | "paused" | "stopped" | "runtimeOffline" | "archived"
    )
}

/// Metadata-only, nonblocking cancellation preflight. No AskStore settlement or
/// publication here: those involve more locks and are recovered by the worker.
/// The callback is still the original dispatcher; it retains its native lane,
/// turn/generation checks and interruption semantics. This is NOT a native bypass.
pub fn dispatch_after_safety_cancel<T>(
    store: &Store,
    session: &str,
    reason: &str,
    dispatch: impl FnOnce() -> Result<T, super::user_input::UserInputExecutionError>,
) -> Result<T, super::user_input::UserInputExecutionError> {
    dispatch_after_safety_cancel_inner(store, session, reason, None, dispatch)
}

fn dispatch_after_safety_cancel_inner<T>(
    store: &Store,
    session: &str,
    reason: &str,
    reservation: Option<&super::session_coordination::OperationReservation<'_>>,
    dispatch: impl FnOnce() -> Result<T, super::user_input::UserInputExecutionError>,
) -> Result<T, super::user_input::UserInputExecutionError> {
    let cancelled = (|| -> anyhow::Result<bool> {
        let lane = super::session_coordination::session_lane(session)?;
        let _reservation = if let Some(reservation) = reservation {
            anyhow::ensure!(reservation.is_current_for(&lane)?, "invalid session operation capability");
            None
        } else {
            Some(lane.try_lock()?)
        };
        store.try_update(|s| s.cancel(session, reason, Utc::now()).is_some())
    })();
    dispatch_after_cancel_result(Some(store), reason, cancelled, dispatch)
}

pub(crate) fn dispatch_after_cancel_result<T>(
    store: Option<&Store>,
    reason: &str,
    result: anyhow::Result<bool>,
    dispatch: impl FnOnce() -> Result<T, super::user_input::UserInputExecutionError>,
) -> Result<T, super::user_input::UserInputExecutionError> {
    if let Err(error) = result {
        if let Some(store) = store {
            if let Err(persistence) = store.disable() {
                eprintln!("OVERLOAD_RETRY_DISABLE_NOT_DURABLE: {persistence:#}; automatic retry remains disabled for this process");
            }
        } else {
            DISABLED_WITHOUT_STORE.store(true, std::sync::atomic::Ordering::Release);
            eprintln!("OVERLOAD_RETRY_DISABLE_NOT_DURABLE: journal unavailable; automatic retry disabled for this process");
        }
        if safety_reason(reason) {
            eprintln!("overload retry disabled; safety operation continues: {error:#}");
        } else {
            return Err(super::server::persistence_user_input_error(error));
        }
    }
    dispatch()
}

pub fn cancel_method(method: &str) -> Option<&'static str> {
    super::quota_continue::cancels_pending(method).or(match method {
        "cutex/asks/pause" | "cutex/userInput/queue/pause" | "cutex/agent/pause" => Some("paused"),
        "turn/interrupt" => Some("ownerStop"),
        "cutex/runtime/restart" => Some("runtimeRestarted"),
        _ => None,
    })
}

pub fn is_path(path: &str) -> bool {
    path == PATH || path.starts_with(&format!("{PATH}/"))
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let session = request
        .path
        .split_once('?')
        .and_then(|(_, q)| q.split('&').find_map(|kv| kv.strip_prefix("sessionId=")))
        .map(str::to_owned);
    let store = Store::open_default()?;
    match request.method.as_str() {
        "GET" if path == PATH => {
            let state = store.snapshot()?;
            let sessions: Vec<_> = match session {
                Some(id) => vec![state.view(&id)],
                None => state.sessions.keys().map(|id| state.view(id)).collect(),
            };
            write_json_response(
                stream,
                200,
                "OK",
                &json!({"hostId":crate::platform::host::current_host_name(),"sessions":sessions}),
            )
        }
        "DELETE" => {
            let id = path
                .strip_prefix(&format!("{PATH}/"))
                .and_then(|s| s.strip_suffix("/pending"));
            if let Some(id) = id.filter(|id| !id.is_empty() && !id.contains('/')) {
                let body = serde_json::from_slice::<Value>(&request.body).ok();
                let valid = body.as_ref().and_then(|b| {
                    let o = b.as_object()?;
                    let plan = o.get("planId")?.as_str()?;
                    let revision = o.get("revision")?.as_u64()?;
                    (o.len() == 2
                        && !plan.trim().is_empty()
                        && plan.len() <= 256
                        && revision <= 9_007_199_254_740_991)
                        .then_some((plan, revision))
                });
                let Some((plan, revision)) = valid else {
                    return super::server::write_v2_error(
                        stream,
                        400,
                        "Bad Request",
                        "invalid_request",
                        "body must contain planId and revision",
                        false,
                        json!({}),
                    );
                };
                if !compare_cancel(
                    &store,
                    &crate::asks::store::AskStore::open_default()?,
                    id,
                    plan,
                    revision,
                )? {
                    return super::server::write_v2_error(
                        stream,
                        409,
                        "Conflict",
                        "stale_plan",
                        "the retry plan changed or already fired",
                        false,
                        json!({}),
                    );
                }
                publish(id, "cancelled");
                write_json_response(
                    stream,
                    200,
                    "OK",
                    &json!({"modelOverloadRetry":store.snapshot()?.view(id)}),
                )
            } else {
                super::server::write_v2_error(
                    stream,
                    404,
                    "Not Found",
                    "not_found",
                    "unknown overload retry route",
                    false,
                    json!({}),
                )
            }
        }
        _ => super::server::write_v2_error(
            stream,
            405,
            "Method Not Allowed",
            "method_not_allowed",
            "method not allowed",
            false,
            json!({}),
        ),
    }
}

pub fn begin_binding(store: &Store, session: &str, plan: &Plan, turn: &str) -> anyhow::Result<()> {
    store
        .update(|s| {
            let p = s.sessions.get_mut(session)?.plan.as_mut()?;
            if p.message_id != plan.message_id
                || !matches!(p.phase, Phase::Uncertain | Phase::Firing | Phase::Binding)
            {
                return None;
            }
            if p.binding_turn_id.as_deref().is_some_and(|id| id != turn) {
                return None;
            }
            p.binding_turn_id = Some(turn.into());
            p.phase = Phase::Binding;
            Some(())
        })?
        .ok_or_else(|| anyhow::anyhow!("retry changed before binding"))
}

pub fn finish_binding(
    store: &Store,
    session: &str,
    plan: &Plan,
    turn: &str,
) -> anyhow::Result<(Option<Value>, Option<Value>)> {
    store.update(|s| {
        let e = s.sessions.get_mut(session)?;
        let p = e.plan.as_mut()?;
        if p.message_id != plan.message_id || p.phase != Phase::Binding || p.binding_turn_id.as_deref() != Some(turn) { return None; }
        e.last = Some(json!({"at":Utc::now().to_rfc3339(),"result":"fired","reason":null,"attempt":plan.attempt}));
        p.turn_id = turn.into(); p.phase = Phase::Running; p.binding_turn_id = None;
        p.lease = p.lease.take().map(|mut l| { l.turn_id=Some(turn.into()); l });
        let early = p.early_terminal.take();
        let message = p.early_message.take();
        let lease = p.lease.clone();
        // Commit any buffered overload together with Running: never erase the
        // only terminal evidence before the next retry/terminal state is durable.
        if let Some(end) = early.as_ref().filter(|v| v["id"].as_str() == Some(turn)) {
            s.terminal(session, &plan.thread_id, plan.runtime_generation, end, lease, Utc::now());
        }
        Some((early, message))
    })?.ok_or_else(|| anyhow::anyhow!("retry changed during binding"))
}

pub fn rebind(
    asks: &crate::asks::store::AskStore,
    session: &str,
    plan: &Plan,
    turn: &str,
) -> anyhow::Result<()> {
    use crate::asks::model::LeaseHolder;
    let changed = asks.with_state(|state| {
        let Some(expected) = &plan.lease else {
            return !state.leases.contains_key(session);
        };
        let Some(lease) = state.leases.get_mut(session) else {
            return false;
        };
        if lease.holder != expected.holder
            || lease.native_message_id != expected.native_message_id
            || lease.runtime_generation != plan.runtime_generation
            || !matches!(lease.turn_id.as_deref(), Some(id) if id == plan.turn_id || id == turn)
        {
            return false;
        }
        // Keep the original event/input identity, not the fresh Continue id.
        match &lease.holder {
            LeaseHolder::Ask { ask_id, attempt } => {
                let Some(record) = state
                    .asks
                    .get_mut(ask_id)
                    .and_then(|a| a.attempts.iter_mut().find(|r| r.n == *attempt))
                else {
                    return false;
                };
                if record.state.is_terminal() || record.stop_requested {
                    return false;
                }
                record.turn_id = Some(turn.into());
                record.ended_at = None;
                record.state = crate::asks::model::AttemptState::Running;
                record.compaction_reminder_pending = false;
            }
            LeaseHolder::Event { event_id } => {
                if let Some(event) = state.events.get_mut(event_id) {
                    event.turn_id = Some(turn.into());
                }
            }
            LeaseHolder::Owner => {}
        }
        lease.turn_id = Some(turn.into());
        lease.since = Utc::now().to_rfc3339();
        true
    })?;
    anyhow::ensure!(
        changed,
        "retry admission lease changed before receipt binding"
    );
    Ok(())
}

#[cfg(test)]
#[path = "model_overload_retry_tests.rs"]
mod tests;
