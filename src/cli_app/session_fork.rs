//! `ManagementRequestContext::fork_session`: fork an agent as a NEW agent
//! (Management `cutex/session/fork`).
//!
//! The fork is made from storage on a private short-lived native app-server
//! on the source session's native home (the same reader offline history
//! uses), so the source runtime, online or not, is never touched:
//!
//! 1. resolve the boundary: `afterTurnId` must be a completed turn of the
//!    source thread (`turn_not_found` / `turn_in_progress`); without it the
//!    newest completed turn (a running turn is left out). The private native
//!    has not loaded the source thread, so it lists a turn the live runtime
//!    is still running as `interrupted`: the runtime's active turn is taken
//!    from the runtime manager instead;
//! 2. native `thread/fork {threadId, lastTurnId}` (inclusive; `beforeTurnId`
//!    only when the sole turn is still running), `excludeTurns: true`. The
//!    boundary is always pinned: the private native reads storage after the
//!    listing, and a busy source may have appended a turn meanwhile. Native
//!    refuses a boundary turn storage still has running (a runtime this
//!    process does not manage, or one that stopped mid-turn); without
//!    `afterTurnId` that turn is then left out too;
//! 3. verify the forked thread was persisted, close the private native
//!    (graceful: stdin closed, it flushes and exits);
//! 4. adopt the forked thread like a newly created one: agent (durable
//!    import) or quick chat (`session_only`) like the source, with the
//!    source's profile, model, reasoning and launch permissions, then make it
//!    visible, name it and add it to the source's project. Its history is
//!    taken from the home it was forked in (the source's native home, not the
//!    selected deployment's), the inherited source history only up to the
//!    byte prefix the fork references, so a live source may keep writing.
use cutex::catalog::{CatalogEndpoint, CatalogError};
use cutex::management::control_plane::HumanManagementPrincipal;
use cutex::management::v2::session_fork::{default_fork_name, SessionForkRequest};
use cutex::management::v2::user_input::UserInputExecutionError;
use cutex::session::model::CutexSessionRecord;
use serde_json::{json, Value};

fn error(code: &str, message: impl Into<String>, retryable: bool, details: Value) -> UserInputExecutionError {
    UserInputExecutionError {
        stage: "route".into(),
        code: code.into(),
        message: message.into(),
        retryable,
        details,
        outcome_unknown: false,
    }
}

fn failed(step: &str, error_text: impl std::fmt::Display, details: Value) -> UserInputExecutionError {
    error(
        "session_fork_failed",
        format!("session fork failed while {step}: {error_text:#}"),
        false,
        details,
    )
}

/// Natives up to r52.1 refuse to fork a thread with an unfinished
/// ExternalInput obligation: a retryable conflict naming what blocks it.
fn fork_blocked_by_external_input(cutex_session_id: &str) -> UserInputExecutionError {
    let details = cutex::app_server::external_recovery::fork_blocked_details(cutex_session_id);
    let message = if details["blockedBy"] == "held" {
        "a message to this agent is held; retry it from the agent's messages, then fork"
    } else {
        "the agent is still working on a message; fork after it finished"
    };
    error("session_fork_blocked_external_input", message, details["blockedBy"] != "held", details)
}

fn unsupported(message: &str) -> UserInputExecutionError {
    error("session_fork_unsupported", message, false, json!({}))
}

/// One turn of the source thread, newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnSummary {
    pub id: String,
    pub in_progress: bool,
}

/// What native `thread/fork` gets and which turn the fork ends with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForkBoundary {
    pub last_turn_id: Option<String>,
    pub before_turn_id: Option<String>,
    pub through_turn_id: Option<String>,
}

/// Boundary for `after_turn_id` (`None`: newest completed turn) over the
/// source's turns, newest first.
pub(crate) fn fork_boundary(
    turns: &[TurnSummary],
    after_turn_id: Option<&str>,
) -> Result<ForkBoundary, UserInputExecutionError> {
    match after_turn_id {
        Some(turn) => {
            let found = turns.iter().find(|candidate| candidate.id == turn).ok_or_else(|| {
                error(
                    "turn_not_found",
                    format!("turn {turn} is not in this thread's history"),
                    false,
                    json!({ "turnId": turn }),
                )
            })?;
            if found.in_progress {
                return Err(error(
                    "turn_in_progress",
                    "the turn is still running; fork after it completed or pick an earlier turn",
                    true,
                    json!({ "turnId": turn }),
                ));
            }
            Ok(ForkBoundary {
                last_turn_id: Some(turn.to_string()),
                before_turn_id: None,
                through_turn_id: Some(turn.to_string()),
            })
        }
        // Pinned even when the newest turn is the boundary: without
        // `lastTurnId` native forks whatever history it reads, which can
        // already hold a turn started after `turns` was listed.
        None => match turns.iter().position(|turn| !turn.in_progress) {
            Some(index) => Ok(ForkBoundary {
                last_turn_id: Some(turns[index].id.clone()),
                before_turn_id: None,
                through_turn_id: Some(turns[index].id.clone()),
            }),
            None => Ok(ForkBoundary {
                last_turn_id: None,
                before_turn_id: turns.last().map(|turn| turn.id.clone()),
                through_turn_id: None,
            }),
        },
    }
}

/// The private native has not loaded the source thread, so it reports a turn
/// the live runtime is still running as `interrupted`: mark the runtime's
/// active turn as running.
pub(crate) fn mark_live_turn(turns: &mut [TurnSummary], live_turn_id: Option<&str>) {
    if let Some(live) = live_turn_id {
        for turn in turns.iter_mut().filter(|turn| turn.id == live) {
            turn.in_progress = true;
        }
    }
}

/// The active turn of the source's live runtime, if this process runs it.
fn live_turn_id(record: &CutexSessionRecord, source_thread: &str) -> Option<String> {
    super::app_server_runtime::runtime_manager()
        .status(&record.cutex_session_id)
        .ok()
        .flatten()
        .filter(|status| status.thread_id == source_thread)
        .and_then(|status| status.active_turn_id)
}

/// Native refuses a `lastTurnId` whose turn has not finished in storage.
fn is_running_turn_refusal(error: &CatalogError) -> bool {
    matches!(error, CatalogError::Rpc { error, .. } if error.message.contains("in-progress turn"))
}

/// Source turns, newest first: all of them when looking for `after_turn_id`
/// (paged), otherwise only the newest page.
fn list_turns(
    endpoint: &mut dyn CatalogEndpoint,
    thread: &str,
    find: Option<&str>,
) -> Result<Vec<TurnSummary>, CatalogError> {
    let mut turns = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen = std::collections::BTreeSet::new();
    loop {
        let page = endpoint.request(
            "thread/turns/list",
            json!({"threadId": thread, "sortDirection": "desc", "limit": 100, "itemsView": "summary","cursor": cursor}),
        )?;
        for turn in page.get("data").and_then(Value::as_array).into_iter().flatten() {
            if let Some(id) = turn.get("id").and_then(Value::as_str) {
                turns.push(TurnSummary {
                    id: id.to_string(),
                    in_progress: turn.get("status").and_then(Value::as_str) == Some("inProgress"),
                });
            }
        }
        let found = find.is_some_and(|wanted| turns.iter().any(|turn| turn.id == wanted));
        cursor = page.get("nextCursor").and_then(Value::as_str).map(str::to_owned);
        let Some(next) = &cursor else { break };
        if found || find.is_none() && turns.iter().any(|turn| !turn.in_progress) || !seen.insert(next.clone()) {
            break;
        }
    }
    Ok(turns)
}

fn launch_permissions(record: &CutexSessionRecord) -> Option<cutex::config::new_thread::LaunchPermissions> {
    let sandbox = record.sandbox_mode.clone().or_else(|| record.permission_defaults.clone())?;
    Some(cutex::config::new_thread::LaunchPermissions {
        sandbox,
        approval: record.approval_policy.clone()?,
        network_access: record.sandbox_network_access.unwrap_or(false),
        writable_roots: record.sandbox_writable_roots.clone(),
    })
}

fn creation_defaults(record: &CutexSessionRecord) -> Option<cutex::agent_management::HumanCreationDefaults> {
    let profile = record.profile.clone().filter(|profile| !profile.trim().is_empty())?;
    let model = record.model_defaults.clone().filter(|model| !model.trim().is_empty())?;
    Some(cutex::agent_management::HumanCreationDefaults {
        profile,
        model,
        reasoning: record.reasoning_defaults.clone(),
        permissions: launch_permissions(record).filter(|permissions| permissions.validate_shape().is_ok()),
    })
}

fn durable_agent(record: &CutexSessionRecord) -> bool {
    record.agent_enabled
        && record.registration_class == cutex::agent_bus::model::AgentRegistrationClass::Persistent
}

struct Forked {
    /// Native home the fork was made in (the source's own home); adoption
    /// takes the new thread's history from here.
    home: std::path::PathBuf,
    thread: String,
    cwd: String,
    through_turn_id: Option<String>,
}

#[cfg(any(target_os = "linux", windows))]
fn fork_native(record: &CutexSessionRecord, source_thread: &str, after_turn_id: Option<&str>) -> Result<Forked, UserInputExecutionError> {
    let home = super::session_history::native_home_for(Some(record))
        .map_err(|e| failed("resolving the native home", e, json!({})))?;
    let mut endpoint = super::session_history::isolated_endpoint(Some(record), &home)
        .map_err(|e| failed("starting a private native app-server", e, json!({})))?;
    let mut turns = list_turns(&mut endpoint, source_thread, after_turn_id)
        .map_err(|e| failed("reading the source history", e, json!({})))?;
    // Read after the listing: a turn listed and still running is then known
    // to the runtime; one that finished in between is complete in storage.
    mark_live_turn(&mut turns, live_turn_id(record, source_thread).as_deref());
    let mut boundary = fork_boundary(&turns, after_turn_id)?;
    let mut retries = 0;
    let response = loop {
        let mut params = json!({ "threadId": source_thread, "excludeTurns": true });
        if let Some(turn) = &boundary.last_turn_id {
            params["lastTurnId"] = json!(turn);
        }
        if let Some(turn) = &boundary.before_turn_id {
            params["beforeTurnId"] = json!(turn);
        }
        match endpoint.request("thread/fork", params) {
            Ok(response) => break response,
            // Storage still has the boundary turn running: a runtime this
            // process does not manage, or one that stopped mid-turn. Without
            // `afterTurnId` leave it out like a listed running turn.
            Err(e) if after_turn_id.is_none() && retries < 2 && is_running_turn_refusal(&e) => {
                retries += 1;
                mark_live_turn(&mut turns, boundary.last_turn_id.as_deref());
                boundary = fork_boundary(&turns, None)?;
            }
            Err(e) => {
                return Err(match &e {
                    _ if is_running_turn_refusal(&e) => error(
                        "turn_in_progress",
                        "the turn has not completed (still running, or stopped mid-turn); fork after it completed or pick an earlier turn",
                        true,
                        json!({ "turnId": boundary.last_turn_id }),
                    ),
                    CatalogError::Rpc { error: rpc, .. }
                        if cutex::app_server::external_recovery::is_fork_blocked_by_external_input(&rpc.message) =>
                    {
                        fork_blocked_by_external_input(&record.cutex_session_id)
                    }
                    CatalogError::Rpc { error: rpc, .. } if rpc.code == -32600 => error(
                        "session_fork_unsupported",
                        format!("native thread/fork refused: {}", rpc.message),
                        false,
                        json!({}),
                    ),
                    _ => failed("forking the native thread", e, json!({})),
                })
            }
        }
    };
    let thread = response
        .pointer("/thread/id")
        .and_then(Value::as_str)
        .filter(|id| uuid::Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == *id))
        .filter(|id| *id != source_thread)
        .ok_or_else(|| failed("forking the native thread", "native returned no new thread id", json!({})))?
        .to_string();
    let read = endpoint
        .request("thread/read", json!({ "threadId": thread, "includeTurns": false }))
        .map_err(|e| failed("verifying the forked thread", e, json!({ "threadId": thread })))?;
    if read.pointer("/thread/id").and_then(Value::as_str) != Some(thread.as_str())
        || read.pointer("/thread/path").and_then(Value::as_str).is_none()
    {
        return Err(failed("verifying the forked thread", "the forked thread was not persisted", json!({ "threadId": thread })));
    }
    let cwd = response
        .get("cwd")
        .and_then(Value::as_str)
        .or_else(|| read.pointer("/thread/cwd").and_then(Value::as_str))
        .map(cutex::agent_management::strip_verbatim_prefix)
        .map(|cwd| cwd.to_string())
        .unwrap_or_else(|| record.cwd.clone());
    // Graceful shutdown: closing stdin lets the native flush the new rollout.
    drop(endpoint);
    Ok(Forked { home, thread, cwd, through_turn_id: boundary.through_turn_id })
}

#[cfg(not(any(target_os = "linux", windows)))]
fn fork_native(_: &CutexSessionRecord, _: &str, _: Option<&str>) -> Result<Forked, UserInputExecutionError> {
    Err(unsupported("session fork requires a local Linux or Windows runtime"))
}

fn current_snapshot(id: &str) -> Result<(u64, bool, u64), String> {
    let store = cutex::session::store::load_cutex_session_store().map_err(|e| format!("{e:#}"))?;
    store
        .sessions
        .get(id)
        .map(|record| (record.durable_revision(), record.exposed_to_backend, record.runtime_generation))
        .ok_or_else(|| "forked session disappeared".to_string())
}

/// Plain desired-state follow-up writes; retried on a racing whole-store CAS.
fn follow_up(id: &str, method: &str, extra: Value) -> Result<(), String> {
    let mut attempt = 0;
    loop {
        let (revision, exposed, _) = current_snapshot(id)?;
        if method == "cutex/session/visibility/show" && exposed {
            return Ok(());
        }
        let mut params = json!({ "expectedRevision": revision });
        if let (Some(params), Some(extra)) = (params.as_object_mut(), extra.as_object()) {
            params.extend(extra.clone());
        }
        match super::management_context::mutate_management_v2_session(id, method, params) {
            Ok(_) => return Ok(()),
            Err(error)
                if attempt < 4
                    && (error.code == "revision_conflict"
                        || error.message.contains("store revision conflict")) =>
            {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(50 * attempt));
            }
            Err(error) => return Err(error.message),
        }
    }
}

pub(crate) fn fork_for_management(
    principal: &HumanManagementPrincipal,
    cutex_session_id: &str,
    request: &SessionForkRequest,
) -> Result<Value, UserInputExecutionError> {
    let store = cutex::session::store::load_cutex_session_store()
        .map_err(|e| failed("loading the session store", e, json!({})))?;
    let key = cutex::session::service::cutex_session_key_for_user_id(&store, cutex_session_id).ok_or_else(|| {
        error(
            "session_not_found",
            "the durable cutex session does not exist",
            false,
            json!({ "cutexSessionId": cutex_session_id }),
        )
    })?;
    let source = store.sessions[&key].clone();
    if source.claude_session.is_some() {
        return super::claude_structure::fork(principal, &source, request);
    }
    if source.is_retired() {
        return Err(unsupported("restore the archived session before forking it"));
    }
    let Some(source_thread) = source.codex_session_id.clone() else {
        return Err(unsupported("the session has no native thread to fork"));
    };
    if !cutex::runtime::lifecycle::cutex_session_host_is_local(
        &source.host_id,
        &cutex::platform::host::current_host_name(),
    ) {
        return Err(unsupported("a session can only be forked on its own host"));
    }
    // The fork is made in the source's own native home; only a selected
    // deployment can give the new session a home of its own (without one,
    // adoption keeps sessions on the host's shared home).
    if source.explicit_launch.is_some()
        && cutex::launch::local_deployment::LocalDeployment::selected().ok().flatten().is_none()
    {
        return Err(unsupported(
            "forking a session with its own runtime home requires a selected local runtime deployment",
        ));
    }
    let agent = durable_agent(&source);
    let taken = |name: &str| {
        store.sessions.values().any(|record| {
            record.formal_agent_name.as_deref() == Some(name)
                || cutex::session::metadata::cutex_session_presented_name(record) == name
        })
    };
    let name = match &request.name {
        Some(name) => {
            if agent && store.sessions.values().any(|r| r.formal_agent_name.as_deref() == Some(name.as_str())) {
                return Err(error(
                    "invalid_request",
                    format!("an agent named {name} already exists"),
                    false,
                    json!({ "name": name }),
                ));
            }
            name.clone()
        }
        None => default_fork_name(&cutex::session::metadata::cutex_session_presented_name(&source), taken),
    };
    // Project of a durable source agent (inherited by the fork).
    let project = if agent {
        let id = cutex::role_revision::CutexSessionId::new(key.clone())
            .map_err(|_| failed("reading the project", "invalid cutex session id", json!({})))?;
        let state = super::management_context::management_agent_provider()
            .and_then(|provider| provider.store().snapshot())
            .map_err(|e| failed("reading the project", e, json!({})))?;
        state
            .agents
            .get(&id)
            .and_then(|record| cutex::agent_management::current_project_id(&state, record))
    } else {
        None
    };

    let forked = fork_native(&source, &source_thread, request.after_turn_id.as_deref())?;
    let thread = forked.thread.clone();
    let partial = |step: &str, message: String, id: Option<&str>| {
        error(
            "session_setup_incomplete",
            format!("forked thread {thread} exists but {step} failed: {message}"),
            false,
            json!({ "threadId": thread, "cutexSessionId": id, "step": step }),
        )
    };
    if let Ok(config) = cutex::config::store::load_codez_config_checked() {
        if let Err(e) = cutex::notify::session::session(
            &thread,
            cutex::notify::session::Change::Set(config.new_session_notification),
        ) {
            eprintln!("session fork: notification initialization failed for {thread}: {e:#}");
        }
    }
    let adopt_request = cutex::agent_management::HumanAdoptRequest {
        session_only: !agent,
        creation_defaults: creation_defaults(&source),
        action_id: cutex::agent_management::AgentActionId::new(format!("session-fork-{thread}"))
            .map_err(|e| partial("adoption", e.to_string(), None))?,
        native_id: thread.clone(),
        cwd: forked.cwd.clone(),
        formal_name: name.clone(),
    };
    let adopted = super::management_context::adopt_forked_native(principal, &adopt_request, &forked.home, source.profile.as_deref())
        .map_err(|e| partial("adoption", e.to_string(), None))?;
    let new_id = adopted.adopted.record.cutex_session_id.clone();
    if let Some(message) = &adopted.error {
        return Err(partial("roster import", message.clone(), Some(&new_id)));
    }
    follow_up(&new_id, "cutex/session/visibility/show", json!({}))
        .map_err(|message| partial("visibility", message, Some(&new_id)))?;
    if !agent {
        follow_up(&new_id, "cutex/session/metadata/update", json!({ "displayName": name }))
            .map_err(|message| partial("display name", message, Some(&new_id)))?;
    }
    if let Some(project) = project {
        use cutex::management::control_plane::{
            HumanManagementProjectMutationKind, HumanManagementProjectMutationRequest,
            HumanManagementProjectMutationSchema,
        };
        let member = cutex::role_revision::CutexSessionId::new(new_id.clone())
            .map_err(|_| partial("project membership", "invalid agent id".into(), Some(&new_id)))?;
        let summary = super::management_context::list_management_projects(principal)
            .map_err(|e| partial("project membership", e.to_string(), Some(&new_id)))?
            .projects
            .into_iter()
            .find(|candidate| candidate.project_id == project);
        if let Some(summary) = summary {
            let action_id = cutex::agent_management::AgentActionId::new(format!("fork-member:{new_id}"))
                .map_err(|e| partial("project membership", e.to_string(), Some(&new_id)))?;
            super::management_context::execute_management_project_mutation(
                principal,
                &HumanManagementProjectMutationRequest {
                    schema: HumanManagementProjectMutationSchema::V1,
                    action_id,
                    project_id: summary.project_id.clone(),
                    expected_authority_epoch: Some(summary.authority_epoch),
                    expected_project_revision: Some(summary.project_revision),
                    operation: HumanManagementProjectMutationKind::AddMember { cutex_session_id: member },
                },
            )
            .map_err(|e| partial("project membership", e.to_string(), Some(&new_id)))?;
        }
    }
    Ok(json!({
        "newCutexSessionId": new_id,
        "sourceThreadId": source_thread,
        "threadId": thread,
        "forkedThroughTurnId": forked.through_turn_id,
        "name": name,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turns(spec: &[(&str, bool)]) -> Vec<TurnSummary> {
        spec.iter()
            .map(|(id, in_progress)| TurnSummary { id: id.to_string(), in_progress: *in_progress })
            .collect()
    }

    #[test]
    fn explicit_turn_must_exist_and_be_complete() {
        let history = turns(&[("t3", true), ("t2", false), ("t1", false)]);
        let boundary = fork_boundary(&history, Some("t2")).unwrap();
        assert_eq!(boundary.last_turn_id.as_deref(), Some("t2"));
        assert_eq!(boundary.through_turn_id.as_deref(), Some("t2"));
        assert_eq!(boundary.before_turn_id, None);
        assert_eq!(fork_boundary(&history, Some("t9")).unwrap_err().code, "turn_not_found");
        assert_eq!(fork_boundary(&history, Some("t3")).unwrap_err().code, "turn_in_progress");
    }

    #[test]
    fn default_boundary_is_the_newest_completed_turn() {
        let idle = turns(&[("t2", false), ("t1", false)]);
        let boundary = fork_boundary(&idle, None).unwrap();
        // Pinned: native must not fork a turn appended after the listing.
        assert_eq!(
            (boundary.last_turn_id.as_deref(), boundary.through_turn_id.as_deref()),
            (Some("t2"), Some("t2"))
        );
        assert_eq!(boundary.before_turn_id, None);
        let running = turns(&[("t3", true), ("t2", false)]);
        let boundary = fork_boundary(&running, None).unwrap();
        assert_eq!(boundary.last_turn_id.as_deref(), Some("t2"));
        let only_running = turns(&[("t1", true)]);
        let boundary = fork_boundary(&only_running, None).unwrap();
        assert_eq!(boundary.before_turn_id.as_deref(), Some("t1"));
        assert_eq!(boundary.through_turn_id, None);
        assert_eq!(
            fork_boundary(&[], None).unwrap(),
            ForkBoundary { last_turn_id: None, before_turn_id: None, through_turn_id: None }
        );
    }

    /// The private native lists the live runtime's running turn as
    /// `interrupted` (it has not loaded the thread); the runtime's active
    /// turn marks it running again, so it is left out of the default boundary
    /// and refused as an explicit one.
    #[test]
    fn live_runtime_turn_is_left_out_of_the_fork() {
        let mut listed = turns(&[("t3", false), ("t2", false), ("t1", false)]);
        mark_live_turn(&mut listed, None);
        mark_live_turn(&mut listed, Some("t9"));
        assert_eq!(listed, turns(&[("t3", false), ("t2", false), ("t1", false)]));
        mark_live_turn(&mut listed, Some("t3"));
        let boundary = fork_boundary(&listed, None).unwrap();
        assert_eq!(boundary.last_turn_id.as_deref(), Some("t2"));
        assert_eq!(boundary.through_turn_id.as_deref(), Some("t2"));
        assert_eq!(fork_boundary(&listed, Some("t3")).unwrap_err().code, "turn_in_progress");
        let mut first_turn = turns(&[("t1", false)]);
        mark_live_turn(&mut first_turn, Some("t1"));
        let boundary = fork_boundary(&first_turn, None).unwrap();
        assert_eq!((boundary.last_turn_id, boundary.before_turn_id.as_deref()), (None, Some("t1")));
    }

    #[test]
    fn native_running_turn_refusal_is_recognized() {
        let rpc = |code, message: &str| CatalogError::Rpc {
            method: "thread/fork".into(),
            error: cutex::app_server::protocol::RpcError { code, message: message.into(), data: None },
        };
        assert!(is_running_turn_refusal(&rpc(-32600, "lastTurnId 't3' identifies an in-progress turn")));
        assert!(is_running_turn_refusal(&rpc(
            -32603,
            "failed to fork thread: lastTurnId 't3' identifies an in-progress turn"
        )));
        assert!(!is_running_turn_refusal(&rpc(-32600, "lastTurnId 't3' not found")));
        assert!(!is_running_turn_refusal(&CatalogError::Transport("in-progress turn".into())));
    }

    #[test]
    fn creation_defaults_copy_profile_model_and_permissions() {
        let mut record = CutexSessionRecord::new_at(
            "cutex.src".into(),
            Some("018a0000-0000-7000-8000-000000000001".into()),
            "host".into(),
            "/tmp".into(),
            Some("work".into()),
            "2026-10-07T00:00:00Z".into(),
        )
        .unwrap();
        assert!(creation_defaults(&record).is_none(), "no model: keep adoption defaults");
        record.model_defaults = Some("gpt-x".into());
        record.reasoning_defaults = Some("high".into());
        let defaults = creation_defaults(&record).unwrap();
        assert_eq!((defaults.profile.as_str(), defaults.model.as_str()), ("work", "gpt-x"));
        assert_eq!(defaults.reasoning.as_deref(), Some("high"));
        assert!(defaults.permissions.is_none());
        record.sandbox_mode = Some("workspace-write".into());
        record.approval_policy = Some("on-request".into());
        record.sandbox_network_access = Some(true);
        let permissions = creation_defaults(&record).unwrap().permissions.unwrap();
        assert_eq!(permissions.sandbox, "workspace-write");
        assert_eq!(permissions.approval, "on-request");
        assert!(permissions.network_access);
    }
}
