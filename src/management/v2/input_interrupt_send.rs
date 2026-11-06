//! `cutex/input/interruptSend` (r274, frozen contract
//! `interrupt-send-contract.md`): the owner interrupts the running turn and
//! sends one of their own inputs as the next turn's first input: exactly
//! once, under the same id, never a text copy.
//!
//! Split:
//! - here (management, lib): params, the CAS and legality checks (pure, also
//!   used by the runtime half under the session lane), the route (switch,
//!   owner, result), the offline start, and which sessions have an
//!   interrupt in flight (the §2.6 delivery shows `finishing` meanwhile);
//! - the runtime half, `cli_app/app_server_interrupt_send.rs`, installed with
//!   [`install`]: everything that touches the runtime and the owner input's
//!   own stores, under the session input lane.
//!
//! Legality (`actions` contains `interruptSend`) is
//! [`input_ledger::interrupt_send_legal`]: the ledger computes it from the
//! session's stored delivery whenever it writes a record, and refreshes the
//! open owner records' actions in the same revision as every delivery
//! change, so `actions` on the wire always match the delivery clients see.

use std::collections::BTreeSet;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use super::input_ledger::{self as il, InputRecord};
use super::user_input::UserInputExecutionError;

pub const METHOD: &str = il::INTERRUPT_SEND_METHOD;
/// `sending{interruptSend}`.
pub const SENDING_REASON: &str = "interruptSend";

/// The request's params (all four required).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterruptSendRequest {
    pub input_id: String,
    pub expected_rev: u64,
    pub expected_turn_id: String,
    pub expected_runtime_generation: u64,
}

/// What the call did (contract "Result.outcome").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The interrupt was issued; the input went out as the next turn's
    /// first input (`started`), or the stopped turn proved it had read it
    /// already (`started: false`, the record is `inContext`).
    Interrupted { turn_id: String, started: bool },
    /// The runtime had it in context already: no interrupt, nothing resent.
    AlreadyInContext,
    /// The host authority fence was busy: `held{hostBusy}`; the owner
    /// admission worker sends it when it gets the fence.
    Held { turn_id: Option<String> },
    /// The runtime went away: `waiting{agentOffline}`, the agent is started
    /// and the input is the first one sent when it is back.
    AgentOffline { turn_id: Option<String> },
}

impl Outcome {
    pub fn wire(&self) -> &'static str {
        match self {
            Self::Interrupted { .. } => "interrupted",
            Self::AlreadyInContext => "alreadyInContext",
            Self::Held { .. } => "held",
            Self::AgentOffline { .. } => "agentOffline",
        }
    }

    pub fn interrupted_turn(&self) -> Option<&str> {
        match self {
            Self::Interrupted { turn_id, .. } => Some(turn_id),
            Self::AlreadyInContext => None,
            Self::Held { turn_id } | Self::AgentOffline { turn_id } => turn_id.as_deref(),
        }
    }
}

/// The runtime half: `(cutexSessionId, bound thread id, request)`.
pub type Handler = fn(&str, &str, &InterruptSendRequest) -> Result<Outcome, UserInputExecutionError>;

static HANDLER: OnceLock<Handler> = OnceLock::new();

/// Install the runtime half (the management process, once).
pub fn install(handler: Handler) {
    let _ = HANDLER.set(handler);
}

pub fn error(code: &str, message: impl Into<String>, details: Value) -> UserInputExecutionError {
    UserInputExecutionError {
        stage: "route".into(),
        code: code.into(),
        message: message.into(),
        retryable: code == "revision_conflict",
        details,
        outcome_unknown: false,
    }
}

fn current_turn(delivery: Option<&Value>) -> Option<&str> {
    delivery?.pointer("/turn/turnId").and_then(Value::as_str)
}

fn current_generation(delivery: Option<&Value>) -> Option<u64> {
    delivery?.get("runtimeGeneration").and_then(Value::as_u64)
}

/// `revision_conflict` with the current values (contract errors table).
pub fn conflict(record: Option<&InputRecord>, delivery: Option<&Value>) -> UserInputExecutionError {
    error(
        "revision_conflict",
        "the input, the running turn or the runtime generation changed; nothing was done",
        json!({
            "currentRev": record.map(|record| record.rev),
            "currentTurnId": current_turn(delivery),
            "currentRuntimeGeneration": current_generation(delivery),
        }),
    )
}

pub fn not_interruptible(record: &InputRecord) -> UserInputExecutionError {
    error(
        "input_not_interruptible",
        format!("{} input in state {} cannot interrupt the running turn", record.kind, record.state),
        json!({"state": record.state, "reason": record.reason}),
    )
}

/// Parse `{inputId, expectedRev, expectedTurnId, expectedRuntimeGeneration}`
/// (a `cutexSessionId`, if given, must name the request's session).
pub fn parse(session: &str, params: &Value) -> Result<InterruptSendRequest, UserInputExecutionError> {
    let invalid = |message: &str| error("invalid_request", message, json!({}));
    let object = params.as_object().ok_or_else(|| invalid("params must be an object"))?;
    if object.keys().any(|key| {
        !matches!(key.as_str(), "inputId" | "expectedRev" | "expectedTurnId" | "expectedRuntimeGeneration" | "cutexSessionId")
    }) {
        return Err(invalid("cutex/input/interruptSend takes {inputId, expectedRev, expectedTurnId, expectedRuntimeGeneration}"));
    }
    if object.get("cutexSessionId").is_some_and(|value| value.as_str() != Some(session)) {
        return Err(invalid("cutexSessionId must be the request's session"));
    }
    let text = |key: &str| {
        object.get(key).and_then(Value::as_str).filter(|value| !value.is_empty()).map(str::to_string)
            .ok_or_else(|| invalid(&format!("{key} must be a non-empty string")))
    };
    let input_id = text("inputId")?;
    let expected_turn_id = text("expectedTurnId")?;
    let expected_rev = il::parse_safe_integer(object.get("expectedRev"), "expectedRev")
        .map_err(|message| invalid(&message))?
        .filter(|rev| *rev > 0)
        .ok_or_else(|| invalid("expectedRev must be a positive integer"))?;
    let expected_runtime_generation = il::parse_safe_integer(object.get("expectedRuntimeGeneration"), "expectedRuntimeGeneration")
        .map_err(|message| invalid(&message))?
        .ok_or_else(|| invalid("expectedRuntimeGeneration is required"))?;
    Ok(InterruptSendRequest { input_id, expected_rev, expected_turn_id, expected_runtime_generation })
}

/// The contract's checks, in order, against the record and the session's
/// current §2.6 delivery (pure; the runtime half runs it under the session
/// lane, so nothing moves in between):
/// 1. no record → `input_not_found`;
/// 2. `rev != expectedRev` → `revision_conflict`;
/// 3. no running turn → `input_not_interruptible`;
/// 4. running turn != `expectedTurnId` or generation !=
///    `expectedRuntimeGeneration` → `revision_conflict`;
/// 5. not legal ([`il::interrupt_send_legal`]: not owner, wrong state, a
///    steer of another turn, a session state without a running turn) →
///    `input_not_interruptible`.
pub fn precheck<'a>(
    record: Option<&'a InputRecord>,
    delivery: Option<&Value>,
    request: &InterruptSendRequest,
) -> Result<&'a InputRecord, UserInputExecutionError> {
    let Some(record) = record else {
        return Err(error("input_not_found", format!("no input {}", request.input_id), json!({"inputId": request.input_id})));
    };
    if record.rev != request.expected_rev {
        return Err(conflict(Some(record), delivery));
    }
    let Some(turn) = current_turn(delivery) else {
        return Err(not_interruptible(record));
    };
    if turn != request.expected_turn_id || current_generation(delivery) != Some(request.expected_runtime_generation) {
        return Err(conflict(Some(record), delivery));
    }
    if !il::interrupt_send_legal(record.kind, record.state, record.reason.as_deref(), record.turn_id.as_deref(), il::shown_to_agent(record), delivery) {
        return Err(not_interruptible(record));
    }
    Ok(record)
}

/// The delivery written with the first transition: the running turn is
/// being interrupted (§2.6 `finishing`, the turn kept).
pub fn finishing(delivery: &Value) -> Value {
    let mut delivery = delivery.clone();
    if let Some(object) = delivery.as_object_mut() {
        object.insert("state".into(), json!("finishing"));
        object.insert("reason".into(), Value::Null);
    }
    delivery
}

// ---------------------------------------------------------------------------
// Interrupts in flight (§2.6 order 7, `finishing`)

fn in_flight() -> std::sync::MutexGuard<'static, BTreeSet<String>> {
    static IN_FLIGHT: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
    IN_FLIGHT.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Whether an interruptSend of `session` has interrupted its turn and not
/// finished yet (the delivery reports `finishing`).
pub fn interrupting(session: &str) -> bool {
    in_flight().contains(session)
}

/// Marks `session` [`interrupting`] until dropped; the delivery is then
/// recomputed.
pub struct Interrupting(String);

impl Interrupting {
    pub fn start(session: &str) -> Self {
        in_flight().insert(session.to_string());
        Self(session.to_string())
    }
}

impl Drop for Interrupting {
    fn drop(&mut self) {
        in_flight().remove(&self.0);
        super::input_ledger_settle::mark_dirty(&self.0);
    }
}

// ---------------------------------------------------------------------------
// Route

/// `cutex/input/interruptSend` (owner only, switch on only). `start_offline`
/// starts the agent when the outcome is `agentOffline` (owner decision 8).
pub(super) fn dispatch(
    owner: bool,
    session_id: &str,
    session: &Value,
    params: &Value,
    start_offline: impl FnOnce(),
) -> Result<Value, UserInputExecutionError> {
    if !il::enabled() {
        return Err(error("cutex_method_denied", "the input ledger is disabled (management.inputLedger.enabled)", json!({})));
    }
    if !owner {
        return Err(error("owner_required", "cutex/input/interruptSend is the owner's (Management root credential)", json!({})));
    }
    let request = parse(session_id, params)?;
    let handler = HANDLER.get().ok_or_else(|| {
        error("app_server_unavailable", "this process cannot reach agent runtimes", json!({}))
    })?;
    let thread = session.pointer("/native/threadId").and_then(Value::as_str).unwrap_or_default();
    let outcome = handler(session_id, thread, &request)?;
    match &outcome {
        Outcome::AgentOffline { .. } => start_offline(),
        // The owner's message started a turn: as for a submit, it ends the
        // delegation pause / stop hold an interrupt-and-send leaves.
        Outcome::Interrupted { started: true, .. } => {
            crate::asks::control::owner_message_default(session_id, "started");
        }
        _ => {}
    }
    super::input_ledger_settle::mark_dirty(session_id);
    let ledger = il::default_ledger().map_err(|error| il::ledger_error(il::LedgerError::Storage(error)))?;
    let latest = ledger.get(session_id, std::slice::from_ref(&request.input_id)).map_err(il::ledger_error)?;
    let record = latest.records.into_iter().next()
        .ok_or_else(|| error("input_not_found", format!("no input {}", request.input_id), json!({"inputId": request.input_id})))?;
    Ok(json!({
        "ledgerEpoch": latest.ledger_epoch,
        "stateRevision": latest.state_revision,
        "outcome": outcome.wire(),
        "interruptedTurnId": outcome.interrupted_turn(),
        "record": record,
    }))
}

#[cfg(test)]
#[path = "input_interrupt_send_tests.rs"]
mod tests;
