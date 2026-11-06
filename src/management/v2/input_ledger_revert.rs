//! r274.2 §9.6: the input ledger follows a confirmed history revert.
//!
//! `cutex/session/history/revert` removes turns from the native thread. Every
//! ledger record of that session bound to one of the confirmed
//! `removedTurnIds` (`turnId` exactly equal) that is `inContext`,
//! `accepted`, `uncertain`, `sending` or an owner `failed` becomes
//! `discarded{historyReverted}` (surface `none`), through the normal batch
//! write (one `stateRevision`, one `cutex/input/stateChanged` per record).
//! Evidence, `turnId` and `timelineItemId` stay as audit;
//! `details.historyRevert = {actionId, turnId}`. Nothing else is touched:
//! kept turns, records without a turn (waiting / nextUp / pendingNotes),
//! other sessions. Never guessed from text, time or page gaps.
//!
//! Crash recovery: the management start (settle engine, once, no timer)
//! reads the persisted revert receipts and applies the same idempotent
//! retraction:
//! - Codex / isolated: `runtime/history-reverts/<action>/receipt.json`
//!   (`status: "complete"`, `threadId` → the one Codex session bound to it);
//! - Claude: `<state_dir>/session-operations/<action>.json`
//!   (`runtime/claude/sessions/<sid>/…`; `state: "completed"`,
//!   `historyCommitted: true`).
//!
//! Both need a non-empty `removedTurnIds`.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::input_ledger::{
    self as il, Expect, InputKind, InputLedger, InputRecord, InputState, LedgerError, Next, Transition,
    HISTORY_REVERTED, MAX_BATCH_ITEMS,
};

/// Log name.
pub const LOG: &str = "cutex-input-ledger-history-revert";
/// `details` key of a retracted record.
pub const DETAILS_KEY: &str = "historyRevert";

/// Whether `record` is retracted by a revert that removed `removed`.
pub fn candidate(record: &InputRecord, removed: &[String]) -> bool {
    let bound = record.turn_id.as_deref().is_some_and(|turn| removed.iter().any(|gone| gone == turn));
    bound
        && match record.state {
            InputState::InContext | InputState::Accepted | InputState::Uncertain | InputState::Sending => true,
            InputState::Failed => record.kind == InputKind::Owner,
            _ => false,
        }
}

/// The `discarded{historyReverted}` write of `record`: its details plus
/// `historyRevert = {actionId, turnId}`.
pub fn retraction(record: &InputRecord, action_id: Option<&str>) -> Next {
    let mut details = if record.details.is_object() { record.details.clone() } else { json!({}) };
    details[DETAILS_KEY] = json!({"actionId": action_id, "turnId": record.turn_id});
    Next::discarded(HISTORY_REVERTED).details(details)
}

/// Retract `session`'s records bound to `removed` (idempotent: a retracted
/// record is terminal and no candidate). Publishes each changed record.
/// A record that moves meanwhile (rev conflict) is read again, at most
/// three times. Returns the changed transitions.
pub fn retract(
    ledger: &InputLedger,
    session: &str,
    removed: &[String],
    action_id: Option<&str>,
) -> Result<Vec<Transition>, LedgerError> {
    let mut out = Vec::new();
    if removed.is_empty() {
        return Ok(out);
    }
    let mut last_conflict = None;
    for _ in 0..3 {
        let records: Vec<InputRecord> =
            ledger.turn_records(session, removed)?.into_iter().filter(|record| candidate(record, removed)).collect();
        if records.is_empty() {
            return Ok(out);
        }
        last_conflict = None;
        for chunk in records.chunks(MAX_BATCH_ITEMS) {
            let items = chunk
                .iter()
                .map(|record| (record.input_id.clone(), Expect::rev(record.rev), retraction(record, action_id)))
                .collect();
            match ledger.transition_batch(session, items) {
                Ok(transitions) => {
                    for transition in transitions.into_iter().filter(|transition| transition.changed) {
                        if let Err(error) = il::publish(&transition) {
                            eprintln!("{LOG}: publish {session}/{}: {error:#}", transition.record.input_id);
                        }
                        out.push(transition);
                    }
                }
                Err(error @ LedgerError::Conflict { .. }) => last_conflict = Some(error),
                Err(error) => return Err(error),
            }
        }
        if last_conflict.is_none() {
            return Ok(out);
        }
    }
    match last_conflict {
        Some(error) => Err(error),
        None => Ok(out),
    }
}

/// `history_revert::dispatch` after the native commit: retract on the
/// process ledger (switch on only). A failure is logged, never returned:
/// the revert is committed, and the startup recovery repairs the ledger.
pub fn after_revert(session: &str, removed: &[String], action_id: Option<&str>) {
    if removed.is_empty() || !il::enabled() {
        return;
    }
    let ledger = match il::default_ledger() {
        Ok(ledger) => ledger,
        Err(error) => {
            eprintln!("{LOG}: {session}: ledger unavailable: {error:#}");
            return;
        }
    };
    match retract(ledger, session, removed, action_id) {
        Ok(transitions) => {
            if !transitions.is_empty() {
                super::input_ledger_settle::mark_dirty(session);
            }
        }
        Err(error) => eprintln!("{LOG}: {session}: retraction after revert {}: {error}", action_id.unwrap_or("?")),
    }
}

// ---------------------------------------------------------------------------
// Startup recovery from persisted receipts

/// A completed, committed revert read from its receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    pub session: String,
    pub action_id: Option<String>,
    pub removed: Vec<String>,
}

/// What the recovery needs of a session record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionView {
    pub id: String,
    pub thread_id: Option<String>,
    /// Claude: the adapter's state directory (`session-operations/` in it).
    pub claude_state_dir: Option<PathBuf>,
    /// A Codex session (neither Claude nor DSH).
    pub codex: bool,
}

impl SessionView {
    pub fn of(record: &crate::session::model::CutexSessionRecord) -> Self {
        Self {
            id: record.cutex_session_id.clone(),
            thread_id: record.app_server_thread_id().map(str::to_string),
            claude_state_dir: record.claude_session.as_ref().map(|session| session.state_dir.clone()),
            codex: record.claude_session.is_none() && record.dsh_session.is_none(),
        }
    }
}

fn removed_turns(receipt: &Value) -> Option<Vec<String>> {
    let turns: Vec<String> = receipt
        .get("removedTurnIds")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .filter(|turn| !turn.is_empty())
        .map(str::to_string)
        .collect();
    (!turns.is_empty()).then_some(turns)
}

/// A Codex `receipt.json` (cli_app `session_history`): `status: "complete"`
/// (written only after the retained history was read back), its `threadId`
/// bound to exactly one Codex session.
pub fn codex_receipt(receipt: &Value, sessions: &[SessionView]) -> Option<Receipt> {
    if receipt.get("status").and_then(Value::as_str) != Some("complete") {
        return None;
    }
    let removed = removed_turns(receipt)?;
    let thread = receipt.get("threadId").and_then(Value::as_str).filter(|thread| !thread.is_empty())?;
    let mut bound = sessions.iter().filter(|session| session.codex && session.thread_id.as_deref() == Some(thread));
    let session = bound.next()?;
    if bound.next().is_some() {
        return None;
    }
    Some(Receipt {
        session: session.id.clone(),
        action_id: receipt.get("actionId").and_then(Value::as_str).map(str::to_string),
        removed,
    })
}

/// A Claude adapter receipt (`session_ops.py` `audit`): `state:
/// "completed"`, `historyCommitted: true`, of this session's thread.
pub fn claude_receipt(receipt: &Value, session: &SessionView) -> Option<Receipt> {
    if receipt.get("state").and_then(Value::as_str) != Some("completed")
        || receipt.get("historyCommitted") != Some(&Value::Bool(true))
    {
        return None;
    }
    let removed = removed_turns(receipt)?;
    if let (Some(thread), Some(own)) = (receipt.get("threadId").and_then(Value::as_str), session.thread_id.as_deref()) {
        if thread != own {
            return None;
        }
    }
    Some(Receipt {
        session: session.id.clone(),
        action_id: receipt.get("actionId").and_then(Value::as_str).map(str::to_string),
        removed,
    })
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Every completed, committed revert receipt on this host: Codex ones under
/// `runtime_dir/history-reverts/`, Claude ones under each Claude session's
/// state directory. Unreadable or other receipts are skipped.
pub fn receipts(runtime_dir: &Path, sessions: &[SessionView]) -> Vec<Receipt> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(runtime_dir.join("history-reverts")) {
        let mut dirs: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        dirs.sort();
        for dir in dirs {
            if let Some(receipt) = read_json(&dir.join("receipt.json")).and_then(|value| codex_receipt(&value, sessions)) {
                out.push(receipt);
            }
        }
    }
    for session in sessions {
        let Some(state_dir) = &session.claude_state_dir else { continue };
        let Ok(entries) = std::fs::read_dir(state_dir.join("session-operations")) else { continue };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
            .collect();
        files.sort();
        for file in files {
            if let Some(receipt) = read_json(&file).and_then(|value| claude_receipt(&value, session)) {
                out.push(receipt);
            }
        }
    }
    out
}

/// Apply every receipt's retraction; `(receipts, retracted records)`.
pub fn recover(ledger: &InputLedger, receipts: &[Receipt]) -> (usize, usize) {
    let mut retracted = 0;
    for receipt in receipts {
        match retract(ledger, &receipt.session, &receipt.removed, receipt.action_id.as_deref()) {
            Ok(transitions) => {
                if !transitions.is_empty() {
                    retracted += transitions.len();
                    super::input_ledger_settle::mark_dirty(&receipt.session);
                }
            }
            Err(error) => eprintln!("{LOG}: {}: recovery of {}: {error}",
                receipt.session, receipt.action_id.as_deref().unwrap_or("?")),
        }
    }
    (receipts.len(), retracted)
}

/// Once per settle engine start (switch on only): list the receipts once,
/// retract, log the counts. No timer, no polling.
#[derive(Debug, Default)]
pub struct Recovery {
    done: bool,
}

impl Recovery {
    pub fn start() -> Self {
        Self { done: false }
    }

    pub fn done(&self) -> bool {
        self.done
    }

    pub fn step(&mut self, ledger: &InputLedger) {
        if std::mem::replace(&mut self.done, true) {
            return;
        }
        let runtime_dir = match crate::config::paths::runtime_dir() {
            Ok(dir) => dir,
            Err(error) => return eprintln!("{LOG}: runtime dir: {error:#}"),
        };
        let sessions: Vec<SessionView> = match crate::session::store::load_cutex_session_store_shared() {
            Ok(store) => store.sessions.values().map(SessionView::of).collect(),
            Err(error) => return eprintln!("{LOG}: session store: {error:#}"),
        };
        let found = receipts(&runtime_dir, &sessions);
        let (count, retracted) = recover(ledger, &found);
        if retracted > 0 {
            eprintln!("{LOG}: retracted {retracted} record(s) from {count} committed revert receipt(s)");
        }
    }
}

#[cfg(test)]
#[path = "input_ledger_revert_tests.rs"]
mod tests;
