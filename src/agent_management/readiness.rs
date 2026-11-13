//! Bounded creation barrier for asynchronously verified Claude occurrences.
//! An absent endpoint is pending; conflicting identity is never pending.
use super::{AgentRuntimeObservation, LifecycleFailure};
use std::time::Duration;

/// Uses an injected monotonic clock and wait so delayed adoption is testable
/// without wall-clock sleeps. `read` must fence the durable process occurrence.
pub fn wait_for_claude_registration_with(
    budget: Duration,
    mut read: impl FnMut() -> Result<AgentRuntimeObservation, LifecycleFailure>,
    elapsed: impl Fn() -> Duration,
    mut wait: impl FnMut(Duration),
) -> Result<AgentRuntimeObservation, LifecycleFailure> {
    let expected = read()?;
    let mut observed = expected.clone();
    loop {
        // All observed spec/native/generation fields stay fixed while waiting.
        let mut baseline = expected.clone();
        baseline.agent_bus_endpoint_ids.clear();
        baseline.profile.clear();
        baseline.runtime_version = None;
        let mut current = observed.clone();
        current.agent_bus_endpoint_ids.clear();
        current.profile.clear();
        current.runtime_version = None;
        if current != baseline {
            return Err(LifecycleFailure::outcome_unknown(
                "claude_readiness_occurrence_changed",
                "Claude occurrence changed during readiness; this readiness wait dispatched no input; inspect the original action for any earlier delivery",
            ));
        }
        if !observed.active
            || !observed.app_server_runtime
            || observed.runtime_generation == 0
            || observed.runtime_agent_ids.len() != 1
        {
            return Err(LifecycleFailure::outcome_unknown(
                "claude_readiness_unavailable",
                "Claude native observation is not ready; this readiness wait dispatched no input; inspect the original action for any earlier delivery",
            ));
        }
        match observed.agent_bus_endpoint_ids.as_slice() {
            [endpoint] if endpoint == &observed.runtime_agent_ids[0] => {
                if elapsed() >= budget {
                    return Err(timeout());
                }
                return Ok(observed);
            }
            [] => {}
            _ => {
                return Err(LifecycleFailure::definite(
                    "claude_readiness_endpoint_mismatch",
                    "Claude readiness has conflicting or duplicate endpoints; this readiness wait dispatched no input; inspect the original action for any earlier delivery",
                ))
            }
        }
        let remaining = budget.saturating_sub(elapsed());
        if remaining.is_zero() {
            return Err(timeout());
        }
        wait(remaining.min(Duration::from_millis(50)));
        if elapsed() >= budget {
            return Err(timeout());
        }
        observed = read()?;
    }
}

fn timeout() -> LifecycleFailure {
    LifecycleFailure::outcome_unknown(
        "claude_readiness_timeout",
        "Exact Claude occurrence registration did not become ready before the deadline; retain this action and inspect it before retrying; this readiness wait dispatched no input; inspect the original action for any earlier delivery",
    )
}
