//! Busy wake scheduling is presentation metadata, not delivery evidence.
//! The existing admission projection supplies policy/holds/frozen timing.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use crate::agent_bus::busy_delivery::{self, Capability};
use super::input_ledger::{self as il, InputKind, InputLedger, InputRecord, Expect};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WakeSchedule {
    pub next_step_at: Option<String>,
    pub wait_secs: Option<f64>,
    pub capability: Capability,
    pub timing: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<String>,
}

fn pending_wake(record: &InputRecord) -> bool {
    matches!(record.kind, InputKind::AgentMessage | InputKind::PeerMessage)
        && record.intent.delivery_mode.is_some_and(|mode| mode != il::DeliveryMode::Passive)
        && !record.state.is_settled() && !il::shown_to_agent(record)
}

pub fn project(record: &InputRecord, busy: bool, capability: Capability,
    row: Option<&super::admission::AdmissionNextStep>, wait: Option<std::time::Duration>, held: Option<&str>) -> Option<WakeSchedule> {
    if !busy || !pending_wake(record) { return None; }
    let row = row?;
    let held = held.map(str::to_string).or_else(|| row.held.clone()).or_else(|| row.wait_reason.clone());
    let soon = capability == Capability::Soon;
    Some(WakeSchedule {
        next_step_at: (soon && held.is_none()).then(|| row.next_step_at.clone()).flatten(),
        wait_secs: wait.map(|wait| wait.as_secs_f64()), capability,
        timing: if soon { row.timing.clone() } else { "afterTurn".into() }, held,
    })
}

/// Called after C5's delivery projection, outside ledger/bus/authority locks.
/// A changed annotation uses the ordinary transactional outbox + CAS; equal
/// annotations consume no revision. State, receipt, intent and deadlines stay.
pub fn refresh(ledger: &InputLedger, session: &str, delivery: &Value) {
    let result = (|| -> anyhow::Result<()> {
        let records = ledger.list(session, il::ListQuery { since_revision: None, include_settled_secs: 0 })?.records;
        if !records.iter().any(|row| pending_wake(row) || row.wake_schedule.is_some()) { return Ok(()); }
        let capability = busy_delivery::for_session(session);
        let busy = delivery.get("runtimeConnected") == Some(&Value::Bool(true)) && !delivery["turn"].is_null();
        let asks = crate::asks::store::AskStore::open_default()?.shared()?;
        let rows = super::agent_bus_state::agent_bus_message_repository()?.pending_admission_messages(session, &asks)?;
        let wait = crate::agent_bus::delivery::wake_busy_wait();
        let held = delivery.get("reason").and_then(Value::as_str).filter(|_| delivery["state"] == "held");
        for record in records {
            let schedule = project(&record, busy, capability, rows.iter().find(|row| row.id == record.input_id), wait, held);
            if record.wake_schedule == schedule { continue; }
            match ledger.annotate_wake_schedule(session, &record.input_id, Expect { rev: Some(record.rev) }, schedule) {
                Ok(change) => il::publish(&change)?,
                Err(il::LedgerError::Conflict { .. } | il::LedgerError::NotFound) => {},
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    })();
    if let Err(error) = result { eprintln!("cutex wake schedule {session}: {error:#}"); }
}
