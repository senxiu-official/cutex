//! Native adapter for typed Agent Management; no nested management HTTP calls.
use super::app_server_runtime::ManagedRuntimeRecoveryOutcome;
use anyhow::ensure;
use cutex::session::model::CutexSessionRecord;

pub(super) fn recover(
    record: &CutexSessionRecord,
) -> anyhow::Result<ManagedRuntimeRecoveryOutcome> {
    if record.app_server_launch_claim_id.is_some() {
        // Leave pending ownership untouched. The authorized online permit uses
        // its stable original action ID; a different action cannot steal it.
        return Ok(ManagedRuntimeRecoveryOutcome::NoClaim);
    }
    let Some(binding) = record.app_server_runtime.as_ref() else {
        ensure!(
            record.runtime_pid.is_none()
                && record.current_runtime_agent_id.is_none()
                && record.alden_pid.is_none(),
            "native ownership has no binding; use human recovery"
        );
        return Ok(ManagedRuntimeRecoveryOutcome::NoClaim);
    };
    if !cutex::platform::process::process_is_running(binding.pid)
        || cutex::platform::process::process_started_at(binding.pid)?.timestamp_millis()
            != chrono::DateTime::parse_from_rfc3339(&binding.started_at)?.timestamp_millis()
    {
        let stopped = super::native_stop::stop_and_commit(record, false)?;
        ensure!(
            stopped.stopped,
            "stale runtime cleanup incomplete: {}",
            stopped.detail
        );
        return Ok(ManagedRuntimeRecoveryOutcome::ClearedDeadClaim);
    }
    super::stock_lifecycle::reconnect_ready_runtime_outside_fence(&record.cutex_session_id)?;
    Ok(ManagedRuntimeRecoveryOutcome::RecoveredExact)
}

pub(super) fn online(
    permit: &cutex::agent_management::RuntimeExecutionPermit<'_>,
    record: &CutexSessionRecord,
) -> anyhow::Result<()> {
    // r273.1 (B1): the live-claim proof of a reconnect runs outside the
    // registered host fence; the claim is compared once the fence is back.
    if record.app_server_runtime.is_some() && record.app_server_launch_claim_id.is_none() {
        super::stock_lifecycle::reconnect_ready_runtime_outside_fence(&record.cutex_session_id)?;
        return Ok(());
    }
    let path = cutex::session::store::cutex_sessions_path()?;
    let receipt = permit.online(&path, &mut super::stock_lifecycle::StockExecutor::default())?;
    ensure!(
        receipt.stage == cutex::agent_management::StockRuntimeStage::Ready
            && receipt.error.is_none(),
        "native start incomplete{}; resume the same managed action (runtime action {})",
        // Why it stopped (e.g. an agent bus timeout), so the failure is classified.
        receipt.error.as_deref().map(|error| format!(" ({error})")).unwrap_or_default(),
        receipt.action_id.as_str()
    );
    super::stock_lifecycle::reconnect_ready_runtime_outside_fence(&record.cutex_session_id)?;
    Ok(())
}
