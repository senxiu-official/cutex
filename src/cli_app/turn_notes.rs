//! Uses the generation-pinned connection already owned by the CLI. No launch,
//! new owner input, Bus wake or model request is used to admit passive notes.
use anyhow::{ensure, Context};
use cutex::app_server::external_input::Response;
use cutex::launch::stock::ExternalInputBinding;
use cutex::app_server::pre_turn_notes::{Budget, Report};

pub(crate) fn synchronize_in_lane(session:&str, thread:&str, generation:Option<u64>) -> Report {
    synchronize_with_budget(session,thread,generation,Budget::default())
}
pub(crate) fn synchronize_with_budget(session:&str, thread:&str, generation:Option<u64>, budget:Budget) -> Report {
    let repository=match cutex::management::v2::agent_bus_state::agent_bus_message_repository() {
        Ok(r)=>r,Err(e)=>return Report {errors:vec![format!("{e:#}")],..Default::default()},
    };
    let captured=match repository.unsubmitted_note_ids(session) {
        Ok(ids)=>ids,Err(e)=>return Report {errors:vec![format!("{e:#}")],..Default::default()},
    };
    if captured.is_empty() {
        use cutex::management::v2::automatic_input::AutomaticInputQueue;
        let has_results = cutex::asks::store::AskStore::open_default()
            .and_then(|store| AutomaticInputQueue::has_startable_results(&store, session))
            .unwrap_or(false);
        if !has_results { return Report::default(); }
    }
    let result = (|| -> anyhow::Result<Report> {
    let record = super::asks_runtime::record(session).context("passive-note recipient missing")?;
    ensure!(record.app_server_thread_id() == Some(thread)
        && generation.is_none_or(|g| g == record.runtime_generation), "passive-note runtime binding changed");
    let generation = record.runtime_generation;
    if captured.is_empty() && record.claude_session.is_none() { return Ok(Report::default()); }
    if record.claude_session.is_none() && record.explicit_launch.is_some() {
        return super::asks_runtime::synchronize_native_notes(session, generation, budget);
    }
    let manager = super::app_server_user_input::session_manager(session);
    let handle = manager.handle_for_generation(session, generation)?;
    let capabilities = if record.claude_session.is_some() {
        super::asks_claude::adapter_capabilities(session, generation).context("passive-note capabilities unavailable")?
    } else { handle.initialize_response().cloned().context("passive-note connection capabilities unavailable")? };
    let ordered = record.claude_session.is_some() && super::claude_runtime::supports_ordered_input(&capabilities);
    let mut report = Report::default();
    let mut ids = captured.clone();
    if ordered {
        // Capture ordinary identity before publishing, since their active
        // reservations intentionally remove them from ordinary eligibility.
        ids.clear();
        for id in &captured {
            if !repository.is_ordinary_automatic_input(session, id)? { ids.push(id.clone()); }
        }
        match super::claude_runtime::synchronize_ordered_start(session, thread, generation, repository, budget) {
            Ok(ordered_report) => report.merge(ordered_report),
            Err(error) => report.errors.push(format!("{error:#}")),
        }
    }
    if ids.is_empty() { return Ok(report); }
    ensure!(capabilities["externalInputVersion"].as_u64() == Some(1),
        "runtime does not support passive-note ingress; note retained for a compatible boundary");
    ensure!(cutex::app_server::external_input::structured_view_supported(&capabilities),
        "runtime cannot accept structured passive notes; note retained for a compatible boundary");
    let binding = ExternalInputBinding { version:1, owner_id:session.into(), thread_id:thread.into(), runtime_generation:generation,
        canonical_byte_limit:super::owner_carries::canonical_policy(session, &record, generation)? };
    let remaining = Budget {
        notes: budget.notes.saturating_sub(report.budget_notes),
        bytes: budget.bytes.saturating_sub(report.budget_bytes),
    };
    let submit = |envelope: &cutex::app_server::external_input::Envelope| {
        manager.handle_for_generation(session, generation)?;
        let response: Response = serde_json::from_value(handle.request("thread/externalInput/submit", serde_json::to_value(envelope)?)?)?;
        response.validate(&binding, &[envelope.key()])?;
        response.statuses.into_iter().next().context("passive-note acceptance missing")
    };
    let legacy = if ordered {
        cutex::app_server::pre_turn_notes::synchronize_budget_for_ids(repository, &binding, remaining, ids, submit)
    } else {
        cutex::app_server::pre_turn_notes::synchronize_budget(repository, &binding, remaining, submit)
    };
    report.merge(legacy);
    Ok(report)
    })();
    match result {
        Ok(report)=>report,
        Err(error)=>Report::unavailable(repository,session,&captured,format!("{error:#}")),
    }
}
