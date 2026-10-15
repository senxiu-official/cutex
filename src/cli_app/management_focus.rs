use cutex::management::v2::model::CutexMessage;
use cutex::management::v2::model::EventCorrelation;
use cutex::management::v2::model::EventSource;
use cutex::management::v2::model::PendingEvent;
use cutex::management::v2::repository::management_v2_repository;
use cutex::management::v2::session::focus_resource;
use cutex::management::v2::session::set_focus;
use cutex::platform::host::current_host_name;
use cutex::session::store::load_cutex_session_store;

pub(crate) fn append_pc_attach_focus_event(
    alden_session_name: &str,
    takeover: bool,
) -> anyhow::Result<()> {
    let store = load_cutex_session_store()?;
    let Some(record) = store
        .sessions
        .values()
        .find(|record| record.alden_session_name.as_deref() == Some(alden_session_name))
    else {
        return Ok(());
    };
    let current = focus_resource(&record.cutex_session_id)?;
    let revision = current
        .get("revision")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(1);
    let focus = set_focus(
        &record.cutex_session_id,
        Some(revision),
        "pc",
        true,
        Some(if takeover {
            "pc_takeover".to_string()
        } else {
            "pc_attach".to_string()
        }),
    )?;
    management_v2_repository()?.append(PendingEvent {
        cutex_session_id: record.cutex_session_id.clone(),
        host_id: current_host_name(),
        source: EventSource::Cutex,
        schema: None,
        correlation: EventCorrelation {
            thread_id: record.codex_session_id.clone(),
            ..Default::default()
        },
        native: None,
        cutex: Some(CutexMessage {
            method: "cutex/focus/changed".to_string(),
            params: focus,
        }),
    })?;
    Ok(())
}
