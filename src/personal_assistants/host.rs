use super::*;
use crate::agent_management::{
    AgentManagementSnapshot, AgentManagementStore, ProjectId, ProjectLifecycle,
};
use crate::management::{control_plane::*, server::ManagementRequestContext};
use anyhow::Result;
use serde_json::{json, Value};

pub fn seat(roster: &AgentManagementSnapshot, project: &str) -> Result<Seat> {
    let id = ProjectId::new(project)?;
    let (authority, state) = crate::agent_management::project_entry(roster, &id)
        .ok_or_else(|| fault("project_not_found"))?;
    require(
        state.lifecycle == ProjectLifecycle::Active,
        "project_inactive",
    )?;
    Ok(Seat {
        session_id: authority.map(|a| a.authorized_director_session.as_str().to_owned()),
        epoch: authority
            .map(|a| a.authority_epoch)
            .or(state.last_director_epoch)
            .unwrap_or(0),
    })
}

pub fn with_directory<T>(read: impl FnOnce(&AgentManagementSnapshot) -> Result<T>) -> Result<T> {
    let directory = AgentManagementStore::open_default()?;
    let _seat_guard = directory.lock_mutations()?;
    let snapshot = directory.directory_snapshot_shared()?;
    read(&snapshot)
}

pub fn validate_scope(config: &Config, roster: &AgentManagementSnapshot) -> Result<()> {
    require(
        config.home_instance_id == crate::management::instance::instance_id(),
        "home_instance_mismatch",
    )?;
    for project in &config.project_ids {
        seat(roster, project)?;
    }
    Ok(())
}

pub(super) fn project_view(roster: &AgentManagementSnapshot, project: &str) -> Result<Value> {
    let director = seat(roster, project)?;
    let id = ProjectId::new(project)?;
    let (_, state) = crate::agent_management::project_entry(roster, &id)
        .ok_or_else(|| fault("project_not_found"))?;
    Ok(
        json!({"projectId":project,"director":director,"revision":state.revision,"lifecycle":state.lifecycle}),
    )
}

fn creation_id(row: &Assistant) -> String {
    format!("personal-assistant-{}", row.id)
}

pub fn provision(
    store: &Store,
    id: &str,
    principal: &HumanManagementPrincipal,
    context: &ManagementRequestContext,
) -> Result<Assistant> {
    let _lock = store.provisioning_lock(id)?;
    let row = store.get(id)?;
    require(row.enabled, "assistant_disabled")?;
    if row.session_id.is_some() {
        return Ok(row);
    }
    let workspace = crate::config::paths::config_dir()?
        .join("personal-assistant-workspaces")
        .join(&row.id);
    crate::management::client_tokens::ensure_private_dir(&workspace)?;
    let workspace = workspace.to_string_lossy().into_owned();
    let project = ProjectId::new(row.coordination_project_id.as_str())?;
    let req = HumanManagementProjectMutationRequest {
        schema: HumanManagementProjectMutationSchema::V1,
        action_id: crate::agent_management::AgentActionId::new(format!("{}-project", row.id))?,
        project_id: project,
        expected_authority_epoch: None,
        expected_project_revision: None,
        operation: HumanManagementProjectMutationKind::CreateProject {
            presentation: crate::agent_management::ProjectPresentationInput {
                display_name: row.config.name.clone(),
                badge_label: "PA".into(),
                color: crate::agent_management::ProjectPaletteColor::Blue,
                icon: None,
            },
            workspace_root: Some(workspace.clone()),
            create_workspace_root: false,
            director_cutex_session_id: None,
        },
    };
    (context.execute_management_project_mutation)(principal, &req)?;
    let action = creation_id(&row);
    let bound_action = format!("human-agent:{action}");
    let active = crate::session::store::session_catalog(false)?;
    let retired = crate::session::store::session_catalog(true)?;
    let bound = crate::session::creation_identity::bound_record(
        active.sessions.values().chain(retired.sessions.values()),
        &bound_action,
    )?;
    let session = if let Some(record) = bound {
        require(
            record.is_active() && record.retired_at.is_none(),
            "creation_binding_inactive",
        )?;
        record.cutex_session_id.clone()
    } else {
        let req = crate::management::v2::human_agents::HumanCreateAgentRequest {
            request_id: action,
            // Formal names are unique on a host. The assistant's owner-facing
            // name is editable independently and may match another assistant.
            name: row.id.clone(),
            project_id: Some(row.coordination_project_id.clone()),
            cwd: Some(workspace),
            profile: Some(row.config.profile.clone()),
            variant_id: None,
            start: false,
            memory_injection: false,
            launch: None,
        };
        req.validate()
            .map_err(|e| anyhow::anyhow!("{}: {}", e.code, e.message))?;
        (context.create_human_agent)(principal, &req)
            .map_err(|e| anyhow::anyhow!("{}: {}", e.code, e.message))?
    };
    let record = crate::session::store::shared_session_row(&session)?
        .ok_or_else(|| fault("creation_binding_incomplete"))?;
    require(
        record.creation_action.as_deref() == Some(bound_action.as_str())
            && record.is_active()
            && record.retired_at.is_none(),
        "creation_binding_incomplete",
    )?;
    // A crash can leave the uniquely correlated agent unassigned. Repair only
    // that incomplete join. An explicit subsequent detach belongs to its owner.
    let sid = crate::role_revision::CutexSessionId::new(session.clone())
        .map_err(|_| fault("creation_binding_incomplete"))?;
    let incomplete = with_directory(|dir| {
        Ok(dir.agents.contains_key(&sid)
            && !dir.current_project_memberships.contains_key(&sid)
            && crate::memory::project_for_session(dir, &session).is_none())
    })?;
    if incomplete {
        let join = HumanManagementProjectMutationRequest {
            schema: HumanManagementProjectMutationSchema::V1,
            action_id: crate::agent_management::AgentActionId::new(format!("{}-join", row.id))?,
            project_id: ProjectId::new(row.coordination_project_id.as_str())?,
            expected_authority_epoch: Some(0),
            expected_project_revision: Some(1),
            operation: HumanManagementProjectMutationKind::AddMember {
                cutex_session_id: sid,
            },
        };
        (context.execute_management_project_mutation)(principal, &join)?;
    }
    // A creation receipt is evidence only after project membership is durable.
    with_directory(|directory| {
        require(
            crate::memory::project_for_session(directory, &session)
                .as_ref()
                .map(ProjectId::as_str)
                == Some(row.coordination_project_id.as_str()),
            "creation_binding_incomplete",
        )?;
        store.bind(&row.id, &session)
    })
}

pub fn dedicated_session(record: &crate::session::model::CutexSessionRecord) -> bool {
    record
        .creation_action
        .as_deref()
        .is_some_and(|a| a.starts_with("human-agent:personal-assistant-pa-"))
}

pub(crate) fn assistant_identity(
    session: &str,
    record: Option<&crate::session::model::CutexSessionRecord>,
) -> Option<bool> {
    if record.is_some_and(dedicated_session) {
        return Some(true);
    }
    // An ordinary canonical session remains usable during an optional registry
    // outage. Without either source, its role cannot be established.
    super::bindings::registered(session).or_else(|| record.map(|_| false))
}

pub fn assistant_session(
    session: &str,
    record: Option<&crate::session::model::CutexSessionRecord>,
) -> bool {
    assistant_identity(session, record) == Some(true)
}

pub fn is_assistant(record: &crate::session::model::CutexSessionRecord) -> bool {
    assistant_session(&record.cutex_session_id, Some(record))
}

/// The scheduler may acquire the alarm lock between disable and outbox cleanup.
/// Read the committed assistant state under that lock before handing off a due
/// self-reminder. This read never takes the assistant write lock.
pub(crate) fn self_reminders_disabled(session: &str) -> Result<bool> {
    let record = crate::session::store::shared_session_row(session)
        .ok()
        .flatten();
    if !assistant_session(session, record.as_deref()) {
        return Ok(false);
    }
    Store::reminders_disabled(session)
}

/// Profile defaults cannot enable execution tools on a coordinator. Older
/// runtime pins remain readable and are refused at launch.
pub fn check_start(record: &crate::session::model::CutexSessionRecord) -> Result<()> {
    if !is_assistant(record) {
        return Ok(());
    }
    Store::open_default()?
        .for_session(&record.cutex_session_id)?
        .require_active()?;
    require(runtime_supported(record), "assistant_runtime_unsupported")
}

pub fn runtime_supported(record: &crate::session::model::CutexSessionRecord) -> bool {
    record.claude_session.as_ref().is_some_and(|session| {
        record.runtime_backend
            == crate::session::model::CutexSessionRuntimeBackend::ClaudeSubscription
            && crate::runtime::claude::adapter_features(&session.deployment.adapter_source)
                ["personalAssistantTools"]
                == true
    })
}

pub(crate) const NOTICE_OWNER: &str = "personal-assistants";
pub(crate) const MISSING_NOTICE_ATTEMPTS: u64 = 8;

/// Storage errors and an uninitialized catalog leave the notice retryable.
/// Existing offline or archived sessions also retain their pending notices.
pub(crate) fn notice_recipient_gone(recipient: &str, attempts: u64) -> bool {
    if attempts < MISSING_NOTICE_ATTEMPTS {
        return false;
    }
    let found = crate::session::store::cutex_sessions_path()
        .and_then(|path| crate::session::store::session_exists_in_catalog_at(&path, recipient));
    matches!(found, Ok(Some(false)))
}

pub fn deliver_pending_notices(cached: &mut Option<Store>) {
    let result = (|| -> Result<()> {
        let store = match cached {
            Some(store) => store,
            None if Store::present()? => cached.insert(Store::open_default()?),
            None => return Ok(()),
        };
        if let Err(e) = store.retry_reminder_cancellations() {
            eprintln!("Personal Assistant reminder cleanup deferred: {e:#}");
        }
        store.deliver_notices(|id, to, text| {
            crate::agent_bus::alarms::set_system_notice(NOTICE_OWNER, id, to, text)?;
            Ok(())
        })
    })();
    if let Err(e) = result {
        *cached = None;
        eprintln!("Personal Assistant notification delivery: {e:#}");
    }
}
