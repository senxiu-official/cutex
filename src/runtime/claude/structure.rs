//! Structural adapter operations never submit a model prompt. An offline
//! operation owns a temporary adapter only; a live adapter keeps its endpoint.
use super::*;

pub const TEMPORARY_OWNER_FILE: &str = "temporary-structural-adapter.json";

fn uncertain_request(error: &crate::app_server::client::AppServerClientError) -> bool {
    use crate::app_server::client::AppServerClientError;
    match error {
        AppServerClientError::Timeout { .. } | AppServerClientError::Disconnected(_)
            | AppServerClientError::Transport(_) => true,
        AppServerClientError::Rpc(error) => error.data.as_ref()
            .and_then(|data| data.get("outcomeUnknown")).and_then(Value::as_bool) == Some(true),
        _ => false,
    }
}

fn temporary_owner(id: &str, session: &ClaudeSession, ready: &Ready, method: &str) -> Value {
    json!({"version": 1, "cutexSessionId": id, "threadId": session.thread_id,
        "operationId": uuid::Uuid::new_v4().to_string(), "method": method,
        "adapterPid": ready.pid, "runtimeId": ready.runtime_id,
        "processStartedAt": crate::platform::process::process_started_at(ready.pid).ok().map(|at| at.to_rfc3339()),
        "recordedAt": Utc::now().to_rfc3339(), "state": "preparing",
        "recovery": "Inspect session-operations and the saved native binding before retrying; a retained adapter may still finish the operation."})
}

fn save_temporary_owner(session: &ClaudeSession, owner: &Value) -> anyhow::Result<()> {
    write_private_pretty_json_atomic(&session.state_dir.join(TEMPORARY_OWNER_FILE), owner, "temporary Claude structural adapter")
}

fn finish_temporary_owner(session: &ClaudeSession, owner: &mut Value, state: &str) {
    owner["state"] = json!(state);
    owner["updatedAt"] = json!(Utc::now().to_rfc3339());
    if let Err(error) = save_temporary_owner(session, owner) {
        eprintln!("Unable to update temporary Claude adapter owner at {}: {error:#}", session.state_dir.join(TEMPORARY_OWNER_FILE).display());
    }
}

/// Only project skills and a dedicated per-account managed catalog are opened.
/// The general Claude home/settings/plugin discovery remains disabled.
pub fn managed_skill_roots(record: &CutexSessionRecord) -> anyhow::Result<Vec<PathBuf>> {
    let session = record
        .claude_session
        .as_ref()
        .context("Claude session missing")?;
    let workspace = Path::new(&record.cwd).canonicalize()?;
    let account = session.deployment.claude_home.canonicalize()?;
    let mut roots = Vec::new();
    for (base, path) in [
        (&workspace, workspace.join(".agents/skills")),
        (&workspace, workspace.join(".claude/skills")),
        (&account, account.join("cutex-skills")),
    ] {
        if path.is_dir() {
            let resolved = path.canonicalize()?;
            if !resolved.starts_with(base) {
                eprintln!("Skipping managed skill root outside its workspace/account: {}", path.display());
                continue;
            }
            // Canonical paths are used for containment, but Python and clients
            // exchange ordinary Windows paths rather than Win32 verbatim ones.
            let public = PathBuf::from(crate::agent_management::strip_verbatim_prefix(
                resolved.to_str().context("managed skill root is not valid Unicode")?,
            ));
            if !roots.contains(&public) {
                roots.push(public);
            }
        }
    }
    Ok(roots)
}

pub fn request(id: &str, method: &str, mut params: Value) -> anyhow::Result<Value> {
    ensure!(
        matches!(method, "claude/thread/fork" | "claude/thread/revert"),
        "unsupported structural operation"
    );
    let initial = record(id)?;
    let _guard = lock(
        initial
            .claude_session
            .as_ref()
            .context("Claude session missing")?,
    )?;
    let record = record(id)?;
    ensure!(!record.is_retired(), "restore the archived agent first");
    ensure!(
        crate::runtime::lifecycle::cutex_session_host_is_local(
            &record.host_id,
            &crate::platform::host::current_host_name()
        ),
        "use the session's own host"
    );
    let session = record
        .claude_session
        .as_ref()
        .context("Claude session missing")?;
    let feature = if method == "claude/thread/revert" {
        "historyRevert"
    } else {
        "fork"
    };
    ensure!(
        adapter_features(&session.deployment.adapter_source)[feature] == true,
        "this adapter does not support {feature}; install and repin to claude-r8"
    );
    params["threadId"] = json!(session.thread_id);
    let temporary = connect(session).is_err();
    let (client, ready) = ensure_server(&record)?;
    let mut owner = temporary.then(|| temporary_owner(id, session, &ready, method));
    if let Some(owner) = &owner {
        if let Err(error) = save_temporary_owner(session, owner) {
            // No structural request has been sent; shutting down our fresh
            // server is safe if its ownership receipt could not be persisted.
            let _ = client.handle().request("claude/server/shutdown", json!({"runtimeId": ready.runtime_id}));
            return Err(error);
        }
    }
    let mut mutation_sent = false;
    let result = (|| -> anyhow::Result<Value> {
        client.handle().request(
            "claude/thread/ensure",
            json!({"threadId": session.thread_id,
            "cwd": record.cwd, "model": session.model, "instructions": session.instructions}),
        )?;
        if let Some(expected) = &session.native_session_id {
            let saved = client.handle().request(
                "claude/thread/inspect",
                json!({"threadId": session.thread_id}),
            )?;
            ensure!(saved["nativeSession"]["session_id"].as_str() == Some(expected),
                "Claude native history mapping changed; inspect the saved adapter state");
        }
        if let Some(owner) = &mut owner {
            owner["state"] = json!("request_pending");
            save_temporary_owner(session, owner)?;
        }
        mutation_sent = true;
        Ok(client.handle().request_with_timeout(method, params, Duration::from_secs(120))?)
    })();
    if let Some(owner) = &mut owner {
        let uncertain = mutation_sent && result.as_ref().err()
            .and_then(|error| error.downcast_ref::<crate::app_server::client::AppServerClientError>())
            .is_some_and(uncertain_request);
        if uncertain {
            finish_temporary_owner(session, owner, "outcome_unknown");
            eprintln!("Claude structural operation outcome is unknown; retained adapter PID {} (runtime {}). Ownership and recovery details: {}",
                ready.pid, ready.runtime_id, session.state_dir.join(TEMPORARY_OWNER_FILE).display());
        } else {
            if let Ok(result) = &result {
                owner["actionId"] = result["actionId"].clone();
                owner["receiptPath"] = result["receiptPath"].clone();
            }
            let shutdown = client.handle().request("claude/server/shutdown", json!({"runtimeId": ready.runtime_id}));
            finish_temporary_owner(session, owner, if shutdown.is_ok() { "shutdown_requested" } else { "shutdown_unconfirmed" });
            if let Err(error) = shutdown {
                eprintln!("Temporary Claude adapter PID {} shutdown is unconfirmed ({error}); ownership recorded at {}",
                    ready.pid, session.state_dir.join(TEMPORARY_OWNER_FILE).display());
            }
        }
    }
    let result = result.with_context(|| if temporary {
        format!("Claude structural adapter PID {} is tracked at {}", ready.pid, session.state_dir.join(TEMPORARY_OWNER_FILE).display())
    } else { "Claude structural operation failed".into() })?;
    let runtime = if method == "claude/thread/revert" {
        &result["runtime"]
    } else {
        &result["sourceRuntime"]
    };
    let native = if method == "claude/thread/revert" {
        Some(
            result["nativeSessionId"]
                .as_str()
                .context("revert omitted native session identity")?,
        )
    } else {
        session.native_session_id.as_deref()
    };
    let connection = runtime["connectionId"].as_str();
    if method == "claude/thread/revert" || connection.is_some() {
        crate::session::store::update_session_row(&cutex_sessions_path()?, id, |current| {
            apply_binding(current, session, native, connection)
        })?
        .context("session removed during revert; inspect the recovery receipt")?;
        if connection.is_some() && !temporary {
            let current = super::record(id)?;
            client.handle().request(
                "claude/externalInput/bind",
                json!({
                    "ownerId": id, "runtimeGeneration": current.runtime_generation
                }),
            )?;
            refresh_mcp_binding(id)?;
        }
    }
    Ok(result)
}

fn apply_binding(
    current: &mut CutexSessionRecord,
    expected: &ClaudeSession,
    native: Option<&str>,
    connection: Option<&str>,
) -> anyhow::Result<()> {
    ensure!(
        current.claude_session.as_ref() == Some(expected),
        "Claude binding changed during history operation; inspect the recovery receipt"
    );
    // Compute every fallible value before mutating the in-memory row.
    let generation = if connection.is_some() && expected.connection_id.as_deref() != connection {
        current
            .runtime_generation
            .checked_add(1)
            .context("generation overflow")?
    } else {
        current.runtime_generation
    };
    let binding = current.claude_session.as_mut().unwrap();
    binding.native_session_id = native.map(str::to_owned);
    if let Some(connection) = connection {
        binding.connection_id = Some(connection.into());
        current.current_runtime_agent_id = Some(format!("claude.{connection}"));
    }
    current.runtime_generation = generation;
    current.updated_at = Utc::now().to_rfc3339();
    Ok(())
}

/// Construct a new, offline agent without inheriting any runtime ownership,
/// queued work, launch claims, or pending deployment changes from the source.
pub fn fork_record(
    source: &CutexSessionRecord,
    id: &str,
    name: &str,
    facts: &Value,
) -> anyhow::Result<CutexSessionRecord> {
    let source_session = source
        .claude_session
        .as_ref()
        .context("Claude source missing")?;
    let thread = facts["threadId"]
        .as_str()
        .context("fork omitted threadId")?;
    let native = facts["nativeSessionId"]
        .as_str()
        .context("fork omitted nativeSessionId")?;
    ensure!(
        uuid::Uuid::parse_str(thread).is_ok() && uuid::Uuid::parse_str(native).is_ok(),
        "invalid fork identities"
    );
    ensure!(
        thread != source_session.thread_id
            && Some(native) != source_session.native_session_id.as_deref(),
        "fork reused source identity"
    );
    let target = crate::config::paths::runtime_dir()?
        .join("claude/sessions")
        .join(id);
    ensure!(
        Path::new(
            facts["stateDir"]
                .as_str()
                .context("fork omitted stateDir")?
        ) == target,
        "fork state directory differs from its allocated destination"
    );
    ensure!(
        target.join("state.sqlite3").is_file(),
        "fork projection was not persisted"
    );
    let mut record = CutexSessionRecord::new(
        id.into(),
        None,
        source.host_id.clone(),
        source.cwd.clone(),
        source.profile.clone(),
    )?;
    record.runtime_backend = CutexSessionRuntimeBackend::ClaudeSubscription;
    record.agent_enabled = source.agent_enabled;
    record.registration_class = source.registration_class;
    record.agent_groups = source.agent_groups.clone();
    record.formal_agent_name = source.formal_agent_name.as_ref().map(|_| name.into());
    record.thread_name = Some(name.into());
    record.display_name_hint = Some(name.into());
    record.exposed_to_backend = true;
    record.model_defaults = source.model_defaults.clone();
    record.reasoning_defaults = source.reasoning_defaults.clone();
    record.permission_defaults = source.permission_defaults.clone();
    record.approval_policy = source.approval_policy.clone();
    record.sandbox_mode = source.sandbox_mode.clone();
    record.sandbox_network_access = source.sandbox_network_access;
    record.sandbox_writable_roots = source.sandbox_writable_roots.clone();
    let mut binding = source_session.clone();
    binding.thread_id = thread.into();
    binding.native_session_id = Some(native.into());
    binding.connection_id = None;
    binding.state_dir = target;
    record.claude_session = Some(binding);
    Ok(record)
}

#[cfg(test)]
#[path = "structure_tests.rs"]
mod tests;
