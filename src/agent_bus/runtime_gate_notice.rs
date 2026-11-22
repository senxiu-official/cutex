//! Source notices for messages held by a failed native turn. Never delivery proof.
use super::{alarms, model::AgentBusMessage};
use crate::management::v2::agent_bus_state::AgentBusMessageRepository;
use crate::app_server::external_input::{HoldReason, ProcessingState, Status};

pub const OWNER: &str = "cutex.runtime-gate-notices";
pub const MESSAGE_PREFIX: &str = "alarm-runtime-gate-";
const HELD: &str = "gate_closed: target last turn failed or was interrupted, needs resume";

/// All notice-only lookups and writes belong inside this auxiliary boundary.
pub fn best_effort(work: impl FnOnce() -> anyhow::Result<()>) {
    if let Err(error) = (|| {
        #[cfg(test)]
        if FAIL_NOTICE.with(|fail| fail.get()) {
            NOTICE_FAILURE_HITS.with(|hits| hits.set(hits.get() + 1));
            anyhow::bail!("injected notice storage failure");
        }
        work()
    })() {
        eprintln!("warning: runtime gate source notice failed: {error:#}");
    }
}

#[cfg(test)]
thread_local! {
    static FAIL_NOTICE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static NOTICE_FAILURE_HITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn with_notice_failure<T>(work: impl FnOnce() -> T) -> T {
    struct Reset(bool);
    impl Drop for Reset { fn drop(&mut self) { FAIL_NOTICE.with(|fail| fail.set(self.0)); } }
    let _reset = Reset(FAIL_NOTICE.with(|fail| fail.replace(true)));
    let before = NOTICE_FAILURE_HITS.with(|hits| hits.get());
    let result = work();
    assert!(NOTICE_FAILURE_HITS.with(|hits| hits.get()) > before, "notice fault injection was not exercised");
    result
}

pub fn held_status(status: &Status) -> bool {
    status.processing.state == ProcessingState::Held
        && matches!(status.processing.reason, Some(HoldReason::Interrupted | HoldReason::RequestUncertain
            | HoldReason::NoOutput | HoldReason::ContextMissing))
}

fn source(message: &AgentBusMessage, alarm_rows: &[alarms::Alarm]) -> Option<String> {
    let source = if message.sender_kind.is_alarm_system() {
        let alarm = alarm_rows.iter().find(|alarm| alarm.message_id == message.id)?;
        &alarm.owner
    } else {
        message.from_cutex_session_id.as_ref()?
    };
    crate::agent_bus::routing::is_full_durable_cutex_session_id(source).then(|| source.clone())
}

fn gate_epoch(state: &crate::asks::store::AskState, target: &str) -> Option<String> {
    if crate::asks::control::gate_resume_queued(state, target) { return None; }
    state.pauses.get(target)
        .filter(|pause| pause.origin.as_deref() == Some(crate::asks::control::ORIGIN_RUNTIME_GATE))
        .map(|pause| pause.since.clone())
}

/// Only a fresh typed native refusal may establish a runtime gate pause.
pub fn record_hold(repository: &AgentBusMessageRepository, message: &AgentBusMessage,
    target: &str, generation: Option<u64>) -> anyhow::Result<()> {
    let store = crate::asks::store::AskStore::open_default()?;
    let (paused, resume_queued) = store.inspect(|state| (state.pauses.contains_key(target), crate::asks::control::gate_resume_queued(state, target)))?;
    if resume_queued { return Ok(()); }
    if !paused {
        let generation = generation.or_else(|| crate::session::store::shared_session_row(target).ok().flatten()
            .map(|record| record.runtime_generation));
        if let Some(generation) = generation {
            store.with_state(|state| {
                if !crate::asks::control::gate_resume_queued(state, target) {
                    crate::asks::control::runtime_gate_pause(state, target, generation, chrono::Utc::now());
                }
            })?;
            crate::management::v2::asks::publish_pending_notices(&store);
        }
    }
    if let Some(epoch) = store.inspect(|state| gate_epoch(state, target))? {
        notify_epoch(repository, message, target, &epoch)?;
    }
    Ok(())
}

fn notify_epoch(repository: &AgentBusMessageRepository, message: &AgentBusMessage,
    target: &str, epoch: &str) -> anyhow::Result<()> {
    notify_epoch_with(repository, message, target, epoch, |owner, key, to, text| {
        alarms::set_system_notice(owner, key, to, text).map(|_| ())
    })
}

fn notify_epoch_with(repository: &AgentBusMessageRepository, message: &AgentBusMessage,
    target: &str, epoch: &str,
    schedule: impl FnOnce(&str, &str, &str, &str) -> anyhow::Result<()>) -> anyhow::Result<()> {
    let Some(snapshot) = repository.snapshot_by_message_id(&message.id)? else { return Ok(()); };
    if !matches!(snapshot.state.as_str(), "pending" | "inbox_accepted") { return Ok(()); }
    if snapshot.error.as_ref().is_some_and(|error| error["runtimeGateNotice"]["target"] == target
        && error["runtimeGateNotice"]["epoch"] == epoch) { return Ok(()); }
    let canonical = repository.canonical_message(&message.id)?;
    let message = &canonical;
    if message.to_cutex_session_id.as_deref() != Some(target) { return Ok(()); }
    repository.record_external_input_error(&message.id, HELD)?;
    if message.sender_kind.is_alarm_system()
        && message.external_message_id.as_deref().unwrap_or(&message.id).starts_with(MESSAGE_PREFIX) { return Ok(()); }
    let rows = if message.sender_kind.is_alarm_system() { alarms::read_all_at(&alarms::store_path()?)? } else { Vec::new() };
    let Some(to) = source(message, &rows) else { return Ok(()); };
    let key = serde_json::to_string(&(&to, target, epoch))?;
    // Identical text for every queued message of this source/target/epoch.
    let text = format!("Delivery from {} to {} is held: target last turn failed or was interrupted, needs resume. Messages remain queued; this is not a read receipt. Director or owner can resume the target with agent_resume / Resume.", to, target);
    schedule(OWNER, &key, &to, &text)?;
    // Mark only after the idempotent alarm write. Failure retries the same key.
    repository.record_runtime_gate_notice(&message.id, &to, target, epoch)?;
    Ok(())
}

/// Cached gate proof can notify, never re-pause a target concurrently resumed.
pub fn report_existing_hold(repository: &AgentBusMessageRepository, message: &AgentBusMessage, target: &str) -> anyhow::Result<()> {
    let store = crate::asks::store::AskStore::open_default()?;
    if let Some(epoch) = store.inspect(|state| gate_epoch(state, target))? {
        notify_epoch(repository, message, target, &epoch)?;
    }
    Ok(())
}

/// Read the gate once per batch, isolating each source's notice failure.
pub fn report_existing_holds(repository: &AgentBusMessageRepository, messages: &[AgentBusMessage], target: &str) -> anyhow::Result<()> {
    if messages.is_empty() { return Ok(()); }
    let store = crate::asks::store::AskStore::open_default()?;
    if let Some(epoch) = store.inspect(|state| gate_epoch(state, target))? {
        for message in messages { best_effort(|| notify_epoch(repository, message, target, &epoch)); }
    }
    Ok(())
}

/// Only never-enqueued notices are retired; queued receipts stay real.
pub fn obsolete(alarm: &alarms::Alarm) -> anyhow::Result<bool> {
    if alarm.owner != OWNER { return Ok(false); }
    let store = crate::asks::store::AskStore::open_default()?;
    if let Ok((_source, target, epoch)) = serde_json::from_str::<(String, String, String)>(&alarm.alarm_id) {
        return store.inspect(|state| gate_epoch(state, &target).as_deref() != Some(&epoch));
    }
    // Legacy per-message notices may survive an upgrade.
    let (target, message): (String, String) = serde_json::from_str(&alarm.alarm_id)?;
    if store.inspect(|state| !crate::asks::control::runtime_gate_closed(state, &target))? { return Ok(true); }
    let repository = crate::management::v2::agent_bus_state::agent_bus_message_repository()?;
    Ok(repository.snapshot_by_message_id(&message)?.is_some_and(|snapshot|
        !matches!(snapshot.state.as_str(), "pending" | "inbox_accepted")))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn queued(repo: &AgentBusMessageRepository, sender: &str, target: &str, id: &str, alarm: bool) -> AgentBusMessage {
        use crate::management::v2::agent_bus_state::AgentBusQueuedMessage;
        let message: AgentBusMessage = serde_json::from_value(serde_json::json!({
            "id":id, "from":if alarm { "Cutex Alarm" } else { "sender" }, "to":"target",
            "fromCutexSessionId":if alarm { None } else { Some(sender) },
            "toCutexSessionId":target, "triggerTurn":true, "createdAtEpochSecs":1,
            "deliveryMode":"after_turn", "senderKind":if alarm { "alarm_system" } else { "agent" }
        })).unwrap();
        repo.record_queued_isolated(AgentBusQueuedMessage {
            owner_cutex_session_id: target.into(), message_id: id.into(), from_cutex_session_id: message.from_cutex_session_id.clone(),
            to_cutex_session_id: target.into(), from_runtime_agent_id: None, to_runtime_agent_id: None,
            delivery_mode: "after_turn".into(), content: message.content.clone(), queued_at: chrono::Utc::now(),
            canonical_envelope: message.clone(), semantic_sha256: "a".repeat(64),
        }).unwrap();
        message
    }

    fn target_notices(target: &str) -> Vec<alarms::Alarm> {
        alarms::read_all_at(&alarms::store_path().unwrap()).unwrap().into_iter()
            .filter(|alarm| alarm.owner == OWNER && serde_json::from_str::<(String,String,String)>(&alarm.alarm_id)
                .is_ok_and(|(_, to, _)| to == target)).collect()
    }

    #[test]
    fn real_store_dedupes_source_target_epoch_without_read_ack_and_retires_old_epoch() {
        let repo = crate::management::v2::agent_bus_state::agent_bus_message_repository().unwrap();
        let target = format!("cutex.{}", uuid::Uuid::new_v4());
        let sender = format!("cutex.{}", uuid::Uuid::new_v4());
        let other = format!("cutex.{}", uuid::Uuid::new_v4());
        let now = chrono::Utc::now();
        let alarm: alarms::Alarm = serde_json::from_value(alarms::set(&sender, alarms::SetArgs {
            alarm_id: uuid::Uuid::new_v4().to_string(), to: target.clone(), message: "source alarm".into(),
            at: None, delay_seconds: Some(3600),
        }).unwrap()).unwrap();
        let mut messages: Vec<_> = (0..5).map(|_| queued(repo, &sender, &target,
            &format!("tell-{}", uuid::Uuid::new_v4()), false)).collect();
        messages.push(queued(repo, &sender, &target, &alarm.message_id, true));
        record_hold(repo, &messages[0], &target, Some(7)).unwrap();
        report_existing_holds(repo, &messages, &target).unwrap();
        report_existing_holds(repo, &messages, &target).unwrap();
        let notices = target_notices(&target);
        assert_eq!(notices.len(), 1, "five tells and the same owner's alarm share one notice");
        assert_eq!(notices[0].to, sender);
        assert!(!obsolete(&notices[0]).unwrap());
        let mut legacy = notices[0].clone();
        legacy.alarm_id = serde_json::to_string(&(&target, &messages[0].id)).unwrap();
        assert!(!obsolete(&legacy).unwrap(), "legacy two-tuple still works");
        for message in &messages {
            let snapshot = repo.snapshot_by_message_id(&message.id).unwrap().unwrap();
            assert_eq!(snapshot.state, "pending");
            assert_eq!(snapshot.error.as_ref().unwrap()["code"], "gate_closed");
            assert!(snapshot.error.as_ref().unwrap()["runtimeGateNotice"]["epoch"].is_string());
            assert!(snapshot.a4_receipt.is_none() && snapshot.external_input_receipt.is_none());
        }
        let message = queued(repo, &other, &target, &format!("tell-{}", uuid::Uuid::new_v4()), false);
        record_hold(repo, &message, &target, Some(7)).unwrap();
        assert_eq!(target_notices(&target).len(), 2, "different source gets its own notice");
        // Pending-error refreshes preserve the successfully scheduled epoch marker.
        repo.record_external_input_error(&messages[0].id, "native pending").unwrap();
        assert!(repo.snapshot_by_message_id(&messages[0].id).unwrap().unwrap().error.unwrap()["runtimeGateNotice"].is_object());
        let (_, _, epoch): (String,String,String) = serde_json::from_str(&notices[0].alarm_id).unwrap();
        notify_epoch_with(repo, &messages[0], &target, &epoch, |_,_,_,_| panic!("scheduled epoch must skip alarm IO")).unwrap();
        let store = crate::asks::store::AskStore::open_default().unwrap();
        store.with_state(|state| crate::asks::control::owner_resume(state, &target, "test", now)).unwrap();
        assert!(obsolete(&notices[0]).unwrap());
        assert!(obsolete(&legacy).unwrap());
        // A cached preflight after resume must not create a fresh pause.
        report_existing_holds(repo, &messages, &target).unwrap();
        assert!(store.inspect(|state| !state.pauses.contains_key(&target)).unwrap());
        store.with_state(|state| {
            state.commands.retain(|command| !matches!(&command.command,
                crate::asks::store::RuntimeCommand::ResumeGate { cutex_session_id } if cutex_session_id == &target));
            crate::asks::control::runtime_gate_pause(state, &target, 8, now + chrono::Duration::seconds(1));
        }).unwrap();
        record_hold(repo, &messages[0], &target, Some(8)).unwrap();
        assert_eq!(target_notices(&target).len(), 3, "new gate epoch gets a fresh notice");
        assert!(obsolete(&notices[0]).unwrap(), "old epoch is stale even while target is held again");
    }

    #[test]
    fn failed_notice_write_leaves_no_marker_and_retry_is_idempotent() {
        let repo = crate::management::v2::agent_bus_state::agent_bus_message_repository().unwrap();
        let target = format!("cutex.{}", uuid::Uuid::new_v4());
        let sender = format!("cutex.{}", uuid::Uuid::new_v4());
        let message = queued(repo, &sender, &target, &format!("tell-{}", uuid::Uuid::new_v4()), false);
        let epoch = "2026-11-14T00:00:00Z";
        assert!(notify_epoch_with(repo, &message, &target, epoch, |_,_,_,_| anyhow::bail!("injected alarm write failure")).is_err());
        let snapshot = repo.snapshot_by_message_id(&message.id).unwrap().unwrap();
        assert_eq!(snapshot.state, "pending");
        assert!(snapshot.error.unwrap().get("runtimeGateNotice").is_none());
        assert!(snapshot.a4_receipt.is_none() && snapshot.external_input_receipt.is_none());
        assert!(target_notices(&target).is_empty());
        // An uncertain successful alarm write must also retry without duplication.
        assert!(notify_epoch_with(repo, &message, &target, epoch, |owner,key,to,text| {
            alarms::set_system_notice(owner,key,to,text)?;
            anyhow::bail!("injected error after durable alarm write")
        }).is_err());
        assert!(repo.snapshot_by_message_id(&message.id).unwrap().unwrap().error.unwrap().get("runtimeGateNotice").is_none());
        assert_eq!(target_notices(&target).len(), 1);
        notify_epoch(repo, &message, &target, epoch).unwrap();
        notify_epoch(repo, &message, &target, epoch).unwrap();
        assert_eq!(target_notices(&target).len(), 1);
    }

    #[test]
    fn source_is_the_actual_tell_sender_or_alarm_owner_never_a_system_notice() {
        let sender = "cutex.00000000-0000-0000-0000-000000000001";
        let target = "cutex.00000000-0000-0000-0000-000000000002";
        let mut message: AgentBusMessage = serde_json::from_value(serde_json::json!({
            "id":"alarm-1", "from":"Cutex Alarm", "to":"target", "triggerTurn":true,
            "createdAtEpochSecs":1, "senderKind":"alarm_system", "fromCutexSessionId":null
        })).unwrap();
        let mut alarm: alarms::Alarm = serde_json::from_value(serde_json::json!({
            "alarm_id":"a", "owner":sender, "to":target, "message":"m", "at":"2026-11-14T00:00:00Z",
            "delay_seconds":null, "message_id":"alarm-1", "state":"queued", "attempts":1,
            "next_attempt":1, "receipt":null
        })).unwrap();
        assert_eq!(source(&message, &[alarm.clone()]).as_deref(), Some(sender));
        alarm.owner = OWNER.into();
        assert!(source(&message, &[alarm]).is_none());
        assert!(source(&message, &[]).is_none());
        message.sender_kind = super::super::model::AgentMessageKind::Agent;
        message.from_cutex_session_id = Some(sender.into());
        assert_eq!(source(&message, &[]).as_deref(), Some(sender));
    }

    #[test]
    fn failure_hold_is_only_a_candidate_not_gate_proof() {
        let mut status: Status = serde_json::from_value(serde_json::json!({
            "messageId":"m", "semanticSha256":"hash", "deliveryState":"unknown", "receipt":null,
            "processing":{"state":"held", "attemptId":"a", "reason":"interrupted"}
        })).unwrap();
        assert!(held_status(&status));
        status.processing.reason = Some(HoldReason::PlanMode);
        assert!(!held_status(&status));
        status.processing.reason = Some(HoldReason::CanonicalSizePolicy);
        assert!(!held_status(&status));
        status.processing.state = ProcessingState::Pending;
        status.processing.reason = Some(HoldReason::NoOutput);
        assert!(!held_status(&status));
    }
}
