//! `cutex/session/profile/switch {profile, expectedRevision, when}`:
//! mid-conversation profile switch, gated per profile.
//!
//! = gate (both profiles allow switching, same CLI kind, same provider family
//! — `provider_family_mismatch` otherwise —, a Claude session stays on its
//! Claude account — `claude_account_mismatch` —, no active turn for `now`) + the ordinary `cutex/session/profile/set` mutation + a runtime
//! restart (`cutex/runtime/offline` then `cutex/runtime/online`) through the
//! same dispatch paths and events as the individual methods. `online`
//! resumes the session's bound native thread, so the thread ID is unchanged
//! (verified after the restart). `afterTurn` with an active turn returns
//! `status: "scheduled"` and performs the switch from a daemon thread once the
//! turn is no longer active (in-memory; lost on daemon restart; one pending
//! switch per session, a newer request replaces it). Emits
//! `cutex/session/profileSwitched` or `cutex/session/profileSwitchFailed`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::management::control_plane::HumanManagementPrincipal;
use crate::management::server::ManagementRequestContext;
use crate::profiles::model::CliKind;
use crate::profiles::settings_api;

use super::repository::EventRepository;
use super::server::{
    append_cutex_event, dispatch_runtime_mutation,
    allowed_params, invalid_user_input_error, post_operation_persistence_error,
    require_session_revision, ValidatedCutexRequest,
};
use super::user_input::UserInputExecutionError;

pub const METHOD: &str = "cutex/session/profile/switch";
const AFTER_TURN_POLL: Duration = Duration::from_millis(1000);
const AFTER_TURN_MAX_WAIT: Duration = Duration::from_secs(6 * 3600);

/// Pending `afterTurn` switches: session ID -> request ID of the newest one.
static PENDING: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn pending() -> &'static Mutex<HashMap<String, String>> {
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The request id of a pending `afterTurn` switch of a session. An ask
/// scheduler treats it as an admission barrier (asks §5.11): the switch
/// applies before the next ask is admitted.
pub fn pending_switch(cutex_session_id: &str) -> Option<String> {
    pending().lock().ok()?.get(cutex_session_id).cloned()
}

fn error(code: &str, message: impl Into<String>, details: Value) -> UserInputExecutionError {
    UserInputExecutionError {
        stage: "route".to_string(),
        code: code.to_string(),
        message: message.into(),
        retryable: false,
        details,
        outcome_unknown: false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum When {
    Now,
    AfterTurn,
}

#[derive(Debug, Clone)]
pub(super) struct SwitchParams {
    pub(super) profile: String,
    pub(super) when: When,
}

pub(super) fn parse_params(params: &Value) -> Result<SwitchParams, UserInputExecutionError> {
    let object = allowed_params(params, &["profile", "when"], &["expectedRevision"])?;
    let profile = object
        .get("profile")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|profile| !profile.is_empty())
        .ok_or_else(|| invalid_user_input_error("profile must be a non-empty string"))?
        .to_string();
    let when = match object.get("when").and_then(Value::as_str) {
        Some("now") => When::Now,
        Some("afterTurn") => When::AfterTurn,
        _ => return Err(invalid_user_input_error("when must be \"now\" or \"afterTurn\"")),
    };
    Ok(SwitchParams { profile, when })
}

/// Facts the gate needs about one profile.
#[derive(Debug, Clone)]
pub(super) struct GateProfile {
    pub(super) name: String,
    pub(super) cli_kind: CliKind,
    pub(super) allow_switch: bool,
    /// Stable provider family (`crate::profiles::providers`).
    pub(super) family: String,
}

/// Pure gate: returns the canonical target name or a typed refusal.
pub(super) fn gate(
    current: Option<&GateProfile>,
    target: Option<&GateProfile>,
    requested: &str,
) -> Result<(), UserInputExecutionError> {
    let current = current.ok_or_else(|| {
        error(
            "profile_switch_disallowed",
            "the agent has no resolvable current profile to switch from",
            json!({ "side": "current" }),
        )
    })?;
    let target = target.ok_or_else(|| {
        // No alias: an old name fails and names what exists now.
        let available = crate::profiles::identity::current_profile_names();
        error(
            "profile_not_found",
            &crate::profiles::identity::unknown_profile_message(requested, &available),
            json!({ "profile": requested, "availableProfiles": available }),
        )
    })?;
    if current.name == target.name {
        return Err(error(
            "profile_unchanged",
            "the agent already uses this profile",
            json!({ "profile": target.name }),
        ));
    }
    compatibility_gate(current, target)?;
    for (side, profile) in [("current", current), ("target", target)] {
        if !profile.allow_switch {
            return Err(error(
                "profile_switch_disallowed",
                format!("profile {} does not allow mid-conversation switching", profile.name),
                json!({ "side": side, "profile": profile.name }),
            ));
        }
    }
    Ok(())
}

fn compatibility_gate(current: &GateProfile, target: &GateProfile) -> Result<(), UserInputExecutionError> {
    compatibility_gate_for(current,target,"switch")
}

fn compatibility_gate_for(current: &GateProfile, target: &GateProfile, operation: &str) -> Result<(), UserInputExecutionError> {
    if current.cli_kind != target.cli_kind {
        return Err(error(
            "profile_switch_cli_mismatch",
            format!(
                "cannot {operation} a {} agent to a {} profile",
                current.cli_kind, target.cli_kind
            ),
            json!({ "from": current.name, "to": target.name,
                    "fromCliKind": current.cli_kind.to_string(), "toCliKind": target.cli_kind.to_string() }),
        ));
    }
    // Owner decision (2026-10-07): a conversation only moves between
    // profiles of the same provider family (provider-specific history items).
    if current.family != target.family {
        return Err(error(
            "provider_family_mismatch",
            format!(
                "cannot {operation} from {} ({}) to {} ({}): different provider families",
                current.name, current.family, target.name, target.family
            ),
            json!({ "from": current.name, "to": target.name,
                    "fromFamily": current.family, "toFamily": target.family,
                    "fromFamilyLabel": crate::profiles::providers::family_label(&current.family),
                    "toFamilyLabel": crate::profiles::providers::family_label(&target.family) }),
        ));
    }
    Ok(())
}

/// Provider-family guard shared by direct profile/set and profile forks.
/// This does not grant an in-place Claude account change.
pub fn family_gate(
    current: &crate::profiles::model::StoredAccount,
    target: &crate::profiles::model::StoredAccount,
) -> Result<(), UserInputExecutionError> {
    family_gate_using(current,target,"switch")
}

fn family_gate_using(current: &crate::profiles::model::StoredAccount, target: &crate::profiles::model::StoredAccount, operation: &str) -> Result<(), UserInputExecutionError> {
    let from = GateProfile {
        name: current.name.clone(),
        cli_kind: current.cli_kind.clone(),
        allow_switch: true,
        family: crate::profiles::providers::provider_family(current).0,
    };
    let to = GateProfile {
        name: target.name.clone(),
        cli_kind: target.cli_kind.clone(),
        allow_switch: true,
        family: crate::profiles::providers::provider_family(target).0,
    };
    compatibility_gate_for(&from, &to, operation)
}

/// A cleared profile retains this already-verified conversation boundary.
pub fn conversation_family_gate(name: &str, family: &str, target: &crate::profiles::model::StoredAccount) -> Result<(), UserInputExecutionError> {
    let from=GateProfile{name:name.into(),cli_kind:CliKind::Codex,allow_switch:true,family:family.into()};
    let to=GateProfile{name:target.name.clone(),cli_kind:target.cli_kind.clone(),allow_switch:true,family:crate::profiles::providers::provider_family(target).0};
    compatibility_gate(&from,&to)
}

/// Resolve an explicitly selected fork profile. Only the target's switch opt-in
/// is required; the source may disallow in-place switching. Account equality
/// is deliberately not required for a new fork.
pub fn fork_target(
    record: &crate::session::model::CutexSessionRecord,
    requested: &str,
) -> Result<crate::profiles::model::StoredAccount, UserInputExecutionError> {
    let store = settings_api::load_store().map_err(store_unavailable)?;
    let current = record
        .profile
        .as_deref()
        .and_then(|name| settings_api::find(&store, name))
        .ok_or_else(|| {
            error(
                "profile_fork_disallowed",
                "the source has no resolvable profile; cannot verify provider compatibility",
                json!({}),
            )
        })?;
    let target = settings_api::find(&store, requested).ok_or_else(|| {
        error(
            "profile_not_found",
            "the requested fork profile does not exist",
            json!({"profile":requested}),
        )
    })?;
    let runtime_kind = if record.claude_session.is_some() {
        CliKind::Claude
    } else {
        CliKind::Codex
    };
    if current.cli_kind != runtime_kind {
        return Err(error(
            "profile_fork_disallowed",
            "the source profile no longer matches its runtime kind",
            json!({}),
        ));
    }
    // A Claude profile may have been rebound since this session was created.
    // Compare the frozen source account, never its profile's new account.
    let mut actual = current.clone();
    if record.claude_session.is_some() {
        let registry = crate::runtime::claude_accounts::load().map_err(store_unavailable)?;
        actual.claude_account =
            crate::runtime::claude_accounts::session_account_id(&registry, record);
    }
    fork_gate(&actual, target)?;
    Ok(target.clone())
}

pub fn fork_gate(
    current: &crate::profiles::model::StoredAccount,
    target: &crate::profiles::model::StoredAccount,
) -> Result<(), UserInputExecutionError> {
    family_gate_using(current,target,"fork")?;
    if !settings_api::allow_session_switch(target) {
        return Err(error("profile_fork_disallowed",format!("profile {} does not allow receiving a fork; enable profile session switching first",target.name),json!({"profile":target.name,"side":"target"})));
    }
    Ok(())
}

/// `claude_account_mismatch` for a profile change that would move a Claude
/// session to another Claude account (`cutex/session/profile/set|clear|switch`).
pub fn claude_account_mismatch_error(
    mismatch: &crate::runtime::claude_accounts::ClaudeAccountMismatch,
) -> UserInputExecutionError {
    error(
        crate::runtime::claude_accounts::MISMATCH_CODE,
        mismatch.to_string(),
        json!({
            "sessionAccount": mismatch.session_account,
            "targetAccount": mismatch.target_account,
            "profile": mismatch.profile,
        }),
    )
}

/// A Claude session never changes account (owner decision 2026-10-08):
/// the target profile must resolve to the account the session runs on
/// (`claudeAccount.id` of the session resource). Same account, other
/// profile (defaults / appearance) stays allowed.
pub(super) fn account_gate(
    session_account: Option<&str>,
    target_account: Option<&str>,
    target: &str,
) -> Result<(), UserInputExecutionError> {
    match (session_account, target_account) {
        (Some(session_account), Some(target_account)) if session_account != target_account => {
            Err(claude_account_mismatch_error(&crate::runtime::claude_accounts::ClaudeAccountMismatch {
                session_account: session_account.to_string(),
                target_account: target_account.to_string(),
                profile: Some(target.to_string()),
            }))
        }
        _ => Ok(()),
    }
}

fn gate_profile(store: &crate::profiles::model::AccountsStore, name: &str) -> Option<GateProfile> {
    settings_api::find(store, name).map(|account| GateProfile {
        name: account.name.clone(),
        cli_kind: account.cli_kind.clone(),
        allow_switch: settings_api::allow_session_switch(account),
        family: crate::profiles::providers::provider_family(account).0,
    })
}

fn current_profile_name(session: &Value) -> Option<String> {
    ["profile", "launchedProfile"]
        .iter()
        .find_map(|key| session.get(*key).and_then(Value::as_str))
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

fn turn_active(session: &Value) -> bool {
    session
        .pointer("/runtime/activeTurnId")
        .is_some_and(|turn| !turn.is_null())
}

fn store_unavailable(error_value: anyhow::Error) -> UserInputExecutionError {
    UserInputExecutionError {
        stage: "route".into(),
        code: "persistence_uncertain".into(),
        message: format!("profile store unavailable: {error_value:#}"),
        retryable: true,
        details: json!({}),
        outcome_unknown: false,
    }
}

/// Validate and gate against a session snapshot; returns (from, to).
fn check(
    session: &Value,
    params: &SwitchParams,
) -> Result<(String, String), UserInputExecutionError> {
    let store = settings_api::load_store().map_err(store_unavailable)?;
    let current_name = current_profile_name(session);
    let current = current_name.as_deref().and_then(|name| gate_profile(&store, name));
    let target = gate_profile(&store, &params.profile);
    gate(current.as_ref(), target.as_ref(), &params.profile)?;
    let target = target.expect("gated");
    if target.cli_kind == CliKind::Claude {
        account_gate(
            session.pointer("/claudeAccount/id").and_then(Value::as_str),
            crate::runtime::claude_accounts::profile_account_id(&store, Some(&target.name)).as_deref(),
            &target.name,
        )?;
    }
    Ok((current.expect("gated").name, target.name))
}

pub(super) fn dispatch(
    event_repository: &EventRepository,
    context: ManagementRequestContext,
    owner: Option<&HumanManagementPrincipal>,
    cutex_session_id: &str,
    session: &Value,
    request: &ValidatedCutexRequest,
) -> Result<Value, UserInputExecutionError> {
    let owner = owner.copied().ok_or_else(|| {
        error(
            "cutex_method_denied",
            "profile switching requires the Management root credential",
            json!({}),
        )
    })?;
    let params = parse_params(&request.params)?;
    require_session_revision(session, request.params.as_object().expect("validated object"))?;
    let (from, to) = check(session, &params)?;
    if turn_active(session) {
        if params.when == When::Now {
            return Err(error(
                "turn_active",
                "a turn is running; retry with when=afterTurn or after it completes",
                json!({ "activeTurnId": session.pointer("/runtime/activeTurnId") }),
            ));
        }
        pending()
            .lock()
            .map_err(|_| invalid_user_input_error("pending switch registry poisoned"))?
            .insert(cutex_session_id.to_string(), request.request_id.clone());
        let (response_from, response_to) = (from.clone(), to.clone());
        let (model, reasoning) = target_defaults(&to);
        let session_id = cutex_session_id.to_string();
        let request_id = request.request_id.clone();
        let thread_id = session.pointer("/native/threadId").and_then(Value::as_str).map(str::to_string);
        let revision = session.get("revision").and_then(Value::as_u64).unwrap_or(1);
        std::thread::Builder::new()
            .name(format!("cutex-profile-switch-{session_id}"))
            .spawn(move || run_after_turn(context, owner, session_id, request_id, from, to))
            .map_err(|e| invalid_user_input_error(&format!("could not schedule switch: {e}")))?;
        return Ok(json!({
            "cutexSessionId": cutex_session_id,
            "status": "scheduled",
            "from": response_from,
            "to": response_to,
            "revision": revision,
            "runtimeRestarted": false,
            "threadId": thread_id,
            "model": model,
            "reasoningEffort": reasoning,
        }));
    }
    perform(event_repository, context, Some(&owner), cutex_session_id, session, &request.request_id, &from, &to)
}

fn load_session(
    context: ManagementRequestContext,
    repository: &EventRepository,
    cutex_session_id: &str,
) -> anyhow::Result<Option<Value>> {
    let registry = (context.load_registry)()?;
    super::session::session_resource_including_hidden(
        cutex_session_id,
        &registry,
        context.load_runtime_status,
        repository,
    )
}

fn run_after_turn(
    context: ManagementRequestContext,
    owner: HumanManagementPrincipal,
    session_id: String,
    request_id: String,
    from: String,
    to: String,
) {
    let Ok(repository) = super::repository::management_v2_repository() else {
        eprintln!("profile switch {request_id}: event repository unavailable");
        return;
    };
    let started = Instant::now();
    let outcome = loop {
        std::thread::sleep(AFTER_TURN_POLL);
        let still_newest = pending()
            .lock()
            .map(|map| map.get(&session_id) == Some(&request_id))
            .unwrap_or(false);
        if !still_newest {
            return; // superseded by a newer switch request
        }
        let session = match load_session(context, repository, &session_id) {
            Ok(Some(session)) => session,
            Ok(None) => break Err(error("session_not_found", "the agent disappeared", json!({}))),
            Err(e) => break Err(store_unavailable(e)),
        };
        if turn_active(&session) {
            if started.elapsed() > AFTER_TURN_MAX_WAIT {
                break Err(error("turn_active", "the turn did not finish in time", json!({})));
            }
            continue;
        }
        // Re-gate against the fresh state: the profile may have changed.
        let fresh = SwitchParams { profile: to.clone(), when: When::Now };
        break match check(&session, &fresh) {
            Ok((current, _)) if current != from => Err(error(
                "revision_conflict",
                format!("the agent's profile changed to {current} before the turn finished"),
                json!({}),
            )),
            Ok(_) => perform(
                repository,
                context,
                Some(&owner),
                &session_id,
                &session,
                &format!("{request_id}:afterTurn"),
                &from,
                &to,
            ),
            Err(e) => Err(e),
        };
    };
    if let Ok(mut map) = pending().lock() {
        if map.get(&session_id) == Some(&request_id) {
            map.remove(&session_id);
        }
    }
    if let Err(failure) = outcome {
        emit_failed(repository, &session_id, &request_id, &from, &to, &failure);
    }
}

fn emit_failed(
    repository: &EventRepository,
    session_id: &str,
    request_id: &str,
    from: &str,
    to: &str,
    failure: &UserInputExecutionError,
) {
    let _ = append_cutex_event(
        repository,
        session_id,
        super::model::EventCorrelation {
            management_request_id: Some(request_id.to_string()),
            ..Default::default()
        },
        "cutex/session/profileSwitchFailed",
        json!({
            "from": from,
            "to": to,
            "error": { "code": failure.code, "message": failure.message },
            "failedAt": chrono::Utc::now().to_rfc3339(),
        }),
    );
}

/// The model resets to the target profile default on switch: the global
/// new-agent default for that profile, else the profile config.toml values.
fn target_defaults(profile: &str) -> (Option<String>, Option<String>) {
    let config = crate::config::store::load_codez_config();
    let configured = config.new_session_defaults.get(profile).cloned().unwrap_or_default();
    if let Some(account) = settings_api::load_store().ok()
        .and_then(|store| settings_api::find(&store, profile).cloned())
        .filter(|account| account.cli_kind == CliKind::Claude)
    {
        return (Some(configured.model.unwrap_or(account.options.claude.model)), None);
    }
    let snapshot = settings_api::load_store()
        .ok()
        .and_then(|store| settings_api::find(&store, profile).cloned())
        .map(|account| settings_api::codex_snapshot(&account))
        .unwrap_or_default();
    (
        configured.model.or(snapshot.model),
        configured.reasoning.or(snapshot.model_reasoning_effort),
    )
}

fn store_snapshot(cutex_session_id: &str) -> Result<(u64, u64, Option<String>), UserInputExecutionError> {
    let store = crate::session::store::load_cutex_session_store().map_err(store_unavailable)?;
    let key = crate::session::service::cutex_session_key_for_user_id(&store, cutex_session_id)
        .ok_or_else(|| error("session_not_found", "the agent disappeared", json!({})))?;
    let record = &store.sessions[&key];
    Ok((
        record.durable_revision(),
        record.runtime_generation,
        record.app_server_thread_id().map(str::to_string),
    ))
}

#[allow(clippy::too_many_arguments)]
fn perform(
    repository: &EventRepository,
    context: ManagementRequestContext,
    owner: Option<&HumanManagementPrincipal>,
    cutex_session_id: &str,
    session: &Value,
    request_id: &str,
    from: &str,
    to: &str,
) -> Result<Value, UserInputExecutionError> {
    let was_online = session.pointer("/runtime/status").and_then(Value::as_str) == Some("online");
    let (_, generation, thread_before) = store_snapshot(cutex_session_id)?;
    let sub = |suffix: &str, method: &str, params: Value| ValidatedCutexRequest {
        request_id: format!("{request_id}:{suffix}"),
        method: method.to_string(),
        params,
    };
    if was_online {
        dispatch_runtime_mutation(
            repository,
            context.mutate_session,
            cutex_session_id,
            session,
            &sub(
                "offline",
                "cutex/runtime/offline",
                json!({ "expectedRuntimeGeneration": generation, "reason": "profile switch" }),
            ),
        )?;
    }
    let (revision, _, _) = store_snapshot(cutex_session_id)?;
    let (model, reasoning) = target_defaults(to);
    let set = (context.mutate_session)(
        cutex_session_id,
        METHOD,
        json!({ "expectedRevision": revision, "profile": to,
                "model": model, "reasoningEffort": reasoning }),
    )
    .and_then(|result| {
        let revision = result.get("revision").and_then(Value::as_u64).ok_or_else(|| {
            invalid_user_input_error("profile switch mutation omitted revision")
        })?;
        append_cutex_event(
            repository,
            cutex_session_id,
            super::model::EventCorrelation {
                management_request_id: Some(format!("{request_id}:set")),
                ..Default::default()
            },
            "cutex/session/profileUpdated",
            json!({
                "sessionRevision": revision,
                "configuredProfile": result.get("configuredProfile").cloned().unwrap_or(Value::Null),
                "updatedAt": chrono::Utc::now().to_rfc3339(),
            }),
        )
        .map_err(post_operation_persistence_error)?;
        Ok(result)
    });
    let restart = |label: &str| -> Result<(), UserInputExecutionError> {
        if !was_online {
            return Ok(());
        }
        let (_, generation, _) = store_snapshot(cutex_session_id)?;
        let current = load_session(context, repository, cutex_session_id)
            .ok()
            .flatten()
            .unwrap_or_else(|| json!({ "runtime": { "runtimeGeneration": generation } }));
        dispatch_runtime_mutation(
            repository,
            |id, _method, params| (context.online_session)(owner, id, params),
            cutex_session_id,
            &current,
            &sub(
                label,
                "cutex/runtime/online",
                json!({ "expectedRuntimeGeneration": generation, "reason": "profile switch",
                        "openVisibleTerminal": false }),
            ),
        )
        .map(|_| ())
    };
    let set_result = match set {
        Ok(result) => result,
        Err(set_error) => {
            // Bring the agent back on its old profile; report the set failure.
            if let Err(restart_error) = restart("online-rollback") {
                eprintln!(
                    "profile switch rollback restart failed for {cutex_session_id}: {}",
                    restart_error.message
                );
            }
            return Err(set_error);
        }
    };
    restart("online")?;
    let (revision, _, thread_after) = store_snapshot(cutex_session_id)?;
    if thread_before != thread_after {
        return Err(UserInputExecutionError {
            stage: "route".into(),
            code: "persistence_uncertain".into(),
            message: "the native thread binding changed during the profile switch".into(),
            retryable: false,
            details: json!({ "threadBefore": thread_before, "threadAfter": thread_after }),
            outcome_unknown: true,
        });
    }
    let revision = set_result
        .get("revision")
        .and_then(Value::as_u64)
        .map_or(revision, |set_revision| set_revision.max(revision));
    // Report the model actually persisted by the mutation (Claude resolves
    // launch defaults there), including when the profile was edited meanwhile.
    let model = set_result.get("model").cloned().unwrap_or_else(|| json!(model));
    append_cutex_event(
        repository,
        cutex_session_id,
        super::model::EventCorrelation {
            thread_id: thread_after.clone(),
            management_request_id: Some(request_id.to_string()),
            ..Default::default()
        },
        "cutex/session/profileSwitched",
        json!({
            "from": from,
            "to": to,
            "sessionRevision": revision,
            "model": model,
            "reasoningEffort": reasoning,
            "runtimeRestarted": was_online,
            "threadId": thread_after,
            "switchedAt": chrono::Utc::now().to_rfc3339(),
        }),
    )
    .map_err(post_operation_persistence_error)?;
    Ok(json!({
        "cutexSessionId": cutex_session_id,
        "status": "switched",
        "from": from,
        "to": to,
        "revision": revision,
        "model": model,
        "reasoningEffort": reasoning,
        "runtimeRestarted": was_online,
        "threadId": thread_after,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(name: &str, cli_kind: CliKind, allow_switch: bool) -> GateProfile {
        family_profile(name, cli_kind, allow_switch, "zhipu")
    }

    fn family_profile(name: &str, cli_kind: CliKind, allow_switch: bool, family: &str) -> GateProfile {
        GateProfile { name: name.into(), cli_kind, allow_switch, family: family.into() }
    }

    #[test]
    fn fork_gate_requires_target_opt_in_and_same_cli_but_not_source_opt_in() {
        let mut source = crate::profiles::lifecycle::new_claude_account("source");
        let mut target = crate::profiles::lifecycle::new_claude_account("target");
        source.options.allow_session_switch = Some(false);
        target.options.allow_session_switch = Some(true);
        assert!(fork_gate(&source, &target).is_ok());
        target.options.allow_session_switch = Some(false);
        assert_eq!(
            fork_gate(&source, &target).unwrap_err().code,
            "profile_fork_disallowed"
        );
        target.options.allow_session_switch = Some(true);
        target.cli_kind = CliKind::Codex;
        assert_eq!(
            fork_gate(&source, &target).unwrap_err().code,
            "profile_switch_cli_mismatch"
        );
        assert!(account_gate(Some("a"), Some("b"), "target").is_err());
    }

    #[test]
    fn fork_refusal_preserves_profile_names_containing_switch() {
        let source=crate::profiles::lifecycle::new_claude_account("switch-source");
        let mut target=crate::profiles::lifecycle::new_claude_account("switch-target");
        target.cli_kind=CliKind::Codex;
        let error=fork_gate(&source,&target).unwrap_err();
        assert!(error.message.starts_with("cannot fork"));
        // CLI refusal has no names, but family/target opt-in refusals must not
        // rewrite arbitrary profile-name substrings either.
        target.cli_kind=CliKind::Claude;
        target.claude_account=Some("ca-unresolved".into());
        target.options.allow_session_switch=Some(true);
        let error=fork_gate(&source,&target).unwrap_err();
        assert_eq!(error.code,"provider_family_mismatch");
        assert!(error.message.contains("switch-source"));
        assert!(error.message.contains("switch-target"));
    }

    #[test]
    fn rebound_profile_name_cannot_bypass_frozen_source_family() {
        let source = family_profile("same", CliKind::Claude, true, "anthropic");
        let target = family_profile("same", CliKind::Claude, true, "custom:relay.example");
        assert_eq!(compatibility_gate(&source, &target).unwrap_err().code, "provider_family_mismatch");
    }

    #[test]
    fn gate_refuses_cross_family_switches() {
        let zai = family_profile("z.ai", CliKind::Codex, true, "zhipu");
        let bigmodel = family_profile("GLM", CliKind::Codex, true, "zhipu");
        let deepseek = family_profile("deepseek", CliKind::Codex, true, "deepseek");
        let local = family_profile("local", CliKind::Codex, true, "custom:127.0.0.1:8080");
        let other_local = family_profile("local2", CliKind::Codex, true, "custom:127.0.0.1:9090");
        let chatgpt_off = family_profile("quince", CliKind::Codex, false, "openai-chatgpt");
        let claude = family_profile("claude", CliKind::Claude, true, "anthropic");
        assert!(gate(Some(&zai), Some(&bigmodel), "GLM").is_ok());
        let failure = gate(Some(&zai), Some(&deepseek), "deepseek").unwrap_err();
        assert_eq!(failure.code, "provider_family_mismatch");
        assert_eq!(failure.details["fromFamily"], "zhipu");
        assert_eq!(failure.details["toFamily"], "deepseek");
        assert_eq!(failure.details["toFamilyLabel"], "DeepSeek");
        assert_eq!(gate(Some(&local), Some(&other_local), "local2").unwrap_err().code, "provider_family_mismatch");
        // Existing gates keep their precedence: CLI kind first, family before allow_switch.
        assert_eq!(gate(Some(&zai), Some(&claude), "claude").unwrap_err().code, "profile_switch_cli_mismatch");
        assert_eq!(gate(Some(&zai), Some(&chatgpt_off), "quince").unwrap_err().code, "provider_family_mismatch");
        assert_eq!(super::super::server::cutex_operation_error_status("provider_family_mismatch", false), 409);
    }

    #[test]
    fn claude_switches_stay_on_the_session_account() {
        // Same account (other defaults / appearance): allowed.
        assert!(account_gate(Some("default"), Some("default"), "Claude Opus").is_ok());
        assert!(account_gate(Some("ca-work"), Some("ca-work"), "Work Sonnet").is_ok());
        // Not a Claude session / unknown target: other gates decide.
        assert!(account_gate(None, Some("ca-work"), "Work").is_ok());
        assert!(account_gate(Some("default"), None, "x").is_ok());
        let failure = account_gate(Some("default"), Some("ca-work"), "Work").unwrap_err();
        assert_eq!(failure.code, "claude_account_mismatch");
        assert_eq!(failure.details["sessionAccount"], "default");
        assert_eq!(failure.details["targetAccount"], "ca-work");
        assert_eq!(failure.details["profile"], "Work");
        assert_eq!(super::super::server::cutex_operation_error_status("claude_account_mismatch", false), 409);
    }

    #[test]
    fn params_are_exact() {
        let parsed =
            parse_params(&json!({ "expectedRevision": 3, "profile": "GLM", "when": "afterTurn" }))
                .unwrap();
        assert_eq!(parsed.profile, "GLM");
        assert_eq!(parsed.when, When::AfterTurn);
        for bad in [
            json!({ "expectedRevision": 3, "profile": "GLM" }),
            json!({ "expectedRevision": 3, "profile": "GLM", "when": "later" }),
            json!({ "expectedRevision": 3, "profile": " ", "when": "now" }),
            json!({ "expectedRevision": 3, "profile": "GLM", "when": "now", "x": 1 }),
        ] {
            assert!(parse_params(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn gate_refuses_with_typed_codes() {
        let deepseek = profile("deepseek", CliKind::Codex, true);
        let glm = profile("GLM", CliKind::Codex, true);
        let chatgpt = profile("quince", CliKind::Codex, false);
        let claude = profile("claude", CliKind::Claude, true);
        assert!(gate(Some(&deepseek), Some(&glm), "GLM").is_ok());
        let code = |result: Result<(), UserInputExecutionError>| result.unwrap_err().code;
        assert_eq!(code(gate(Some(&deepseek), Some(&chatgpt), "quince")), "profile_switch_disallowed");
        assert_eq!(code(gate(Some(&chatgpt), Some(&glm), "GLM")), "profile_switch_disallowed");
        assert_eq!(code(gate(Some(&deepseek), Some(&claude), "claude")), "profile_switch_cli_mismatch");
        assert_eq!(code(gate(Some(&deepseek), None, "nope")), "profile_not_found");
        assert_eq!(code(gate(None, Some(&glm), "GLM")), "profile_switch_disallowed");
        assert_eq!(code(gate(Some(&glm), Some(&glm), "GLM")), "profile_unchanged");
        let side = gate(Some(&deepseek), Some(&chatgpt), "quince").unwrap_err().details["side"].clone();
        assert_eq!(side, "target");
    }

    #[test]
    fn switch_events_append_to_the_repository() {
        let root = std::env::temp_dir().join(format!("cutex-v2-switch-{}", uuid::Uuid::new_v4()));
        let repository = EventRepository::open(&root, crate::platform::host::current_host_name())
            .expect("open event repository");
        append_cutex_event(
            &repository,
            "cutex.s1",
            super::super::model::EventCorrelation {
                thread_id: Some("thread-1".into()),
                management_request_id: Some("switch-1".into()),
                ..Default::default()
            },
            "cutex/session/profileSwitched",
            json!({ "from": "deepseek", "to": "GLM", "sessionRevision": 9,
                    "runtimeRestarted": true, "threadId": "thread-1",
                    "model": "glm-5.3", "reasoningEffort": "max",
                    "switchedAt": chrono::Utc::now().to_rfc3339() }),
        )
        .expect("append profileSwitched");
        let failure = error("turn_active", "x", json!({}));
        emit_failed(&repository, "cutex.s1", "switch-2", "deepseek", "GLM", &failure);
        let page = repository.page(super::super::repository::ReplayQuery::default()).unwrap();
        assert_eq!(page.events.len(), 2);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn switch_result_and_events_match_the_registry() {
        let result = json!({
            "cutexSessionId": "s1", "status": "switched", "from": "deepseek", "to": "GLM",
            "revision": 9, "runtimeRestarted": true, "threadId": "thread-1", "model": "glm-5.3", "reasoningEffort": null,
        });
        super::super::contract_validation::validate_cutex_response(&json!({
            "contractVersion": 2,
            "requestId": "switch-1",
            "cutexSessionId": "s1",
            "cutex": { "method": METHOD, "result": result },
        }))
        .expect("switch response matches the registry");
        super::super::contract_validation::validate_cutex_event_message(&json!({
            "method": "cutex/session/profileSwitched",
            "params": { "from": "deepseek", "to": "GLM", "sessionRevision": 9,
                        "runtimeRestarted": true, "threadId": "thread-1",
                        "model": "glm-5.3", "reasoningEffort": "max",
                        "switchedAt": "2026-10-06T00:00:00Z" },
        }))
        .expect("profileSwitched matches the registry");
        super::super::contract_validation::validate_cutex_event_message(&json!({
            "method": "cutex/session/profileSwitchFailed",
            "params": { "from": "deepseek", "to": "GLM",
                        "error": { "code": "turn_active", "message": "x" },
                        "failedAt": "2026-10-06T00:00:00Z" },
        }))
        .expect("profileSwitchFailed matches the registry");
        assert!(turn_active(&json!({ "runtime": { "activeTurnId": "t" } })));
        assert!(!turn_active(&json!({ "runtime": { "activeTurnId": null } })));
    }
}
