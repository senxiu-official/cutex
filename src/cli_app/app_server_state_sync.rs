use chrono::Utc;
use serde_json::Value;

use cutex::app_server::client::AppServerEvent;
use cutex::app_server::manager::AppServerRuntimeEventContext;
use cutex::app_server::protocol::RpcNotification;
use cutex::session::service::persist_cutex_session_store_and_im_record;
use cutex::session::store::load_cutex_session_store;

use cutex::management::v2::server_requests as app_server_pending_requests;

pub(crate) fn handle_runtime_event(
    context: &AppServerRuntimeEventContext,
    event: &AppServerEvent,
) -> anyhow::Result<()> {
    match event {
        AppServerEvent::Notification(notification) => {
            apply_notification_side_effects(
                &context.cutex_session_id,
                context.runtime_generation,
                notification,
            )?;
        }
        AppServerEvent::ServerRequest(request) => {
            app_server_pending_requests::record_request(
                &context.cutex_session_id,
                context.runtime_generation,
                request.raw.clone(),
            )?;
        }
        AppServerEvent::ProtocolViolation { .. } => {}
        AppServerEvent::Disconnected { .. } => {
            app_server_pending_requests::clear_session(&context.cutex_session_id)?;
            // A transport disconnect does not prove that the persisted
            // manager-owned process or its visible TUI peer exited. Keep the
            // binding and generation for the bounded same-binding recovery
            // decision made by `cutex/runtime/online`; stale cleanup is only
            // safe after an explicit liveness check there.
        }
    }
    Ok(())
}

fn apply_notification_side_effects(
    cutex_session_id: &str,
    runtime_generation: u64,
    notification: &RpcNotification,
) -> anyhow::Result<()> {
    match notification.method.as_str() {
        "serverRequest/resolved" => {
            if let Some(request_id) = notification
                .params
                .as_ref()
                .and_then(|params| params.get("requestId"))
            {
                let _ = app_server_pending_requests::resolve_request(
                    cutex_session_id,
                    runtime_generation,
                    request_id,
                )?;
            }
        }
        "turn/completed" => {
            // A request still pending when its turn ends can no longer be
            // answered (session resource `runtime.pendingServerRequests`).
            if let Some(turn_id) = notification.params.as_ref().and_then(|params| {
                params
                    .pointer("/turn/id")
                    .or_else(|| params.get("turnId"))
                    .and_then(Value::as_str)
            }) {
                app_server_pending_requests::resolve_turn_requests(
                    cutex_session_id,
                    runtime_generation,
                    turn_id,
                )?;
            }
        }
        "thread/name/updated" => {
            update_session_record(cutex_session_id, runtime_generation, notification)?;
        }
        "thread/settings/updated" => {
            update_session_record(cutex_session_id, runtime_generation, notification)?;
        }
        _ => {}
    }
    Ok(())
}

fn update_session_record(
    cutex_session_id: &str,
    runtime_generation: u64,
    notification: &RpcNotification,
) -> anyhow::Result<()> {
    let mut store = load_cutex_session_store()?;
    let key = session_store_key(&store, cutex_session_id)?;
    let record = store
        .sessions
        .get_mut(&key)
        .ok_or_else(|| anyhow::anyhow!("cutex session disappeared: {key}"))?;
    if !apply_durable_notification(
        record,
        runtime_generation,
        notification,
        &Utc::now().to_rfc3339(),
    )? {
        return Ok(());
    }
    persist_cutex_session_store_and_im_record(&store, &key)
}

fn apply_durable_notification(
    record: &mut cutex::session::model::CutexSessionRecord,
    runtime_generation: u64,
    notification: &RpcNotification,
    timestamp: &str,
) -> anyhow::Result<bool> {
    if record.is_retired() || record.runtime_generation != runtime_generation {
        return Ok(false);
    }
    let changed = match notification.method.as_str() {
        "thread/name/updated" => {
            let thread_name = notification
                .params
                .as_ref()
                .and_then(|params| params.get("threadName"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string);
            let mut changed = false;
            // Quick chats (owned sessions) have no managed identity: their
            // sidebar name follows the native (auto-generated) thread title.
            // A Human displayNameOverride still wins at presentation time.
            if record.is_owned_session() && thread_name.is_some() {
                changed |= replace_if_changed(&mut record.display_name_hint, thread_name.clone());
            }
            changed | replace_if_changed(&mut record.thread_name, thread_name)
        }
        "thread/settings/updated" => {
            let Some(settings) = notification
                .params
                .as_ref()
                .and_then(|params| params.get("threadSettings"))
            else {
                return Ok(false);
            };
            let mut changed = false;
            if let Some(cwd) = settings.get("cwd").and_then(Value::as_str) {
                // The runtime reports the cwd it was launched with; a Claude
                // adapter started on a `\\?\F:\x` record cwd echoes that
                // verbatim spelling. Same directory = no change (r244: the
                // verbatim text replaced the managed cwd and every managed
                // check then failed with `mismatch: cwd`); a real move is
                // stored without the verbatim prefix.
                let launch = cutex::session::metadata::cutex_session_launch_cwd(record).to_string();
                let managed = if cutex::session::metadata::same_session_cwd(cwd, &launch) {
                    // Pin the current spelling (as before for an unpinned record).
                    record.managed_cwd.clone().or(Some(launch))
                } else {
                    Some(cutex::session::metadata::session_cwd_identity(cwd))
                };
                changed |= replace_if_changed(&mut record.managed_cwd, managed);
            }
            if let Some(model) = settings.get("model").and_then(Value::as_str) {
                changed |= replace_if_changed(&mut record.model_defaults, Some(model.to_string()));
            }
            if let Some(effort) = settings.get("effort").and_then(Value::as_str) {
                changed |=
                    replace_if_changed(&mut record.reasoning_defaults, Some(effort.to_string()));
            }
            // session-fast-mode: the native forgets the tier on resume, so the
            // record keeps the last one (null = standard / profile config).
            match settings.get("serviceTier") {
                Some(Value::String(tier)) if !tier.trim().is_empty() => {
                    changed |= replace_if_changed(&mut record.service_tier, Some(tier.clone()));
                }
                Some(Value::Null) => {
                    changed |= replace_if_changed(&mut record.service_tier, None);
                }
                _ => {}
            }
            if let Some(policy) = settings.get("approvalPolicy").and_then(Value::as_str) {
                changed |=
                    replace_if_changed(&mut record.approval_policy, Some(policy.to_string()));
            }
            // The reviewer the owner picks on a running thread (desktop 「自动」)
            // must survive the next start / resume, which sends the record's.
            if let Some(reviewer) = settings
                .get("approvalsReviewer")
                .and_then(Value::as_str)
                .and_then(cutex::config::new_thread::canonical_approvals_reviewer)
            {
                changed |= replace_if_changed(
                    &mut record.approvals_reviewer,
                    Some(reviewer.to_string()),
                );
            }
            if let Some(profile_id) = settings
                .pointer("/activePermissionProfile/id")
                .and_then(Value::as_str)
            {
                // cutex's own extras profile is the workspace profile plus the
                // record's network / writable-root fields; keep the alias stable
                // so the next launch reviews it as workspace-write.
                let profile_id = if profile_id
                    == cutex::launch::stock::CUTEX_WORKSPACE_PERMISSION_PROFILE
                {
                    ":workspace"
                } else {
                    profile_id
                };
                changed |= replace_if_changed(
                    &mut record.permission_defaults,
                    Some(profile_id.to_string()),
                );
                changed |= replace_if_changed(&mut record.sandbox_mode, None);
            } else if let Some(sandbox_type) = settings
                .pointer("/sandboxPolicy/type")
                .and_then(Value::as_str)
            {
                changed |= replace_if_changed(
                    &mut record.sandbox_mode,
                    Some(camel_to_kebab(sandbox_type)),
                );
                // A running workspace-write policy carries its extras; the
                // next start / resume rebuilds them from the record
                // (`apply_workspace_extras`, stock launches), so keep them in
                // step. Other policies leave them (they only apply to
                // workspace-write); a named permission profile (branch above)
                // defines its own.
                if sandbox_type == "workspaceWrite" {
                    changed |= sync_workspace_extras(record, &settings["sandboxPolicy"]);
                }
                // The preset alias must follow, or the next launch sees an
                // alias for the old sandbox next to the new explicit one.
                changed |= cutex::runtime::args::reconcile_permission_fields(
                    record,
                    cutex::runtime::args::PermissionWrite::Sandbox,
                );
            }
            changed
        }
        _ => false,
    };
    if changed {
        record.bump_durable_revision()?;
        record.updated_at = timestamp.to_string();
    }
    Ok(changed)
}

/// `networkAccess` / `writableRoots` of a workspace-write `sandboxPolicy` into
/// the record (absent keys keep the stored value). Roots: non-empty, unique,
/// without the thread cwd (always writable).
fn sync_workspace_extras(
    record: &mut cutex::session::model::CutexSessionRecord,
    policy: &Value,
) -> bool {
    let mut changed = false;
    if let Some(network) = policy.get("networkAccess").and_then(Value::as_bool) {
        changed |= replace_if_changed(&mut record.sandbox_network_access, Some(network));
    }
    if let Some(roots) = policy.get("writableRoots").and_then(Value::as_array) {
        let cwd = record
            .managed_cwd
            .as_deref()
            .map(cutex::session::metadata::normalize_session_cwd);
        let mut kept: Vec<String> = Vec::new();
        for root in roots.iter().filter_map(Value::as_str).map(str::trim) {
            if root.is_empty()
                || cwd.as_deref() == Some(cutex::session::metadata::normalize_session_cwd(root).as_str())
                || kept.iter().any(|existing| existing == root)
            {
                continue;
            }
            kept.push(root.to_string());
        }
        changed |= replace_if_changed(&mut record.sandbox_writable_roots, kept);
    }
    changed
}

fn replace_if_changed<T: PartialEq>(slot: &mut T, value: T) -> bool {
    if *slot == value {
        false
    } else {
        *slot = value;
        true
    }
}

fn session_store_key(
    store: &cutex::session::model::CutexSessionStore,
    cutex_session_id: &str,
) -> anyhow::Result<String> {
    if store.sessions.contains_key(cutex_session_id) {
        return Ok(cutex_session_id.to_string());
    }
    store
        .sessions
        .iter()
        .find_map(|(key, record)| {
            (record.cutex_session_id == cutex_session_id).then(|| key.clone())
        })
        .ok_or_else(|| anyhow::anyhow!("unknown cutex session: {cutex_session_id}"))
}

fn camel_to_kebab(value: &str) -> String {
    let mut result = String::new();
    for character in value.chars() {
        if character.is_ascii_uppercase() {
            if !result.is_empty() {
                result.push('-');
            }
            result.push(character.to_ascii_lowercase());
        } else {
            result.push(character);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> cutex::session::model::CutexSessionRecord {
        let mut record = cutex::session::model::CutexSessionRecord::new_at(
            "cutex.state-sync".to_string(),
            Some("thread-state-sync".to_string()),
            "devbox".to_string(),
            "/tmp/state-sync".to_string(),
            None,
            "2026-08-10T00:00:00Z".to_string(),
        )
        .expect("record");
        record.runtime_generation = 7;
        record
    }

    fn thread_name_notification(name: &str) -> RpcNotification {
        RpcNotification {
            method: "thread/name/updated".to_string(),
            params: Some(serde_json::json!({ "threadName": name })),
            raw: serde_json::json!({}),
        }
    }

    #[test]
    fn owned_session_name_follows_thread_title_but_agents_keep_theirs() {
        let mut owned = record();
        owned.runtime_backend = cutex::session::model::CutexSessionRuntimeBackend::ClaudeSubscription;
        owned.display_name_hint = Some("Session 1a2b3c4d".into());
        assert!(owned.is_owned_session());
        assert!(apply_durable_notification(&mut owned, 7, &thread_name_notification("Trip ideas"), "t").unwrap());
        assert_eq!(owned.display_name_hint.as_deref(), Some("Trip ideas"));
        assert_eq!(owned.thread_name.as_deref(), Some("Trip ideas"));
        assert_eq!(cutex::session::metadata::cutex_session_presented_name(&owned), "Trip ideas");
        owned.display_name_override = Some("Pinned name".into());
        assert!(apply_durable_notification(&mut owned, 7, &thread_name_notification("Other"), "t").unwrap());
        assert_eq!(cutex::session::metadata::cutex_session_presented_name(&owned), "Pinned name");

        let mut agent = record();
        agent.registration_class = cutex::agent_bus::model::AgentRegistrationClass::Persistent;
        agent.formal_agent_name = Some("Worker".into());
        agent.display_name_hint = Some("Worker".into());
        assert!(apply_durable_notification(&mut agent, 7, &thread_name_notification("Generated"), "t").unwrap());
        assert_eq!(agent.display_name_hint.as_deref(), Some("Worker"));
    }

    #[test]
    fn service_tier_follows_thread_settings_and_survives_unrelated_updates() {
        let settings = |value: serde_json::Value| RpcNotification {
            method: "thread/settings/updated".to_string(),
            params: Some(serde_json::json!({ "threadSettings": value })),
            raw: serde_json::json!({}),
        };
        let mut record = record();
        assert!(apply_durable_notification(&mut record, 7, &settings(serde_json::json!({ "serviceTier": "priority" })), "t").unwrap());
        assert_eq!(record.service_tier.as_deref(), Some("priority"));
        // A notification without the key (older native / other fields) keeps it.
        assert!(!apply_durable_notification(&mut record, 7, &settings(serde_json::json!({ "model": null })), "t").unwrap());
        assert_eq!(record.service_tier.as_deref(), Some("priority"));
        // Fast off: the native reports the explicit standard sentinel.
        assert!(apply_durable_notification(&mut record, 7, &settings(serde_json::json!({ "serviceTier": "default" })), "t").unwrap());
        assert_eq!(record.service_tier.as_deref(), Some("default"));
        // null = no tier selected: follow the profile config again.
        assert!(apply_durable_notification(&mut record, 7, &settings(serde_json::json!({ "serviceTier": null })), "t").unwrap());
        assert_eq!(record.service_tier, None);
        // A stale runtime generation never writes.
        assert!(!apply_durable_notification(&mut record, 6, &settings(serde_json::json!({ "serviceTier": "priority" })), "t").unwrap());
        assert_eq!(record.service_tier, None);
    }

    #[test]
    fn approvals_reviewer_follows_thread_settings() {
        let settings = |value: serde_json::Value| RpcNotification {
            method: "thread/settings/updated".to_string(),
            params: Some(serde_json::json!({ "threadSettings": value })),
            raw: serde_json::json!({}),
        };
        let mut record = record();
        assert!(apply_durable_notification(&mut record, 7, &settings(serde_json::json!({ "approvalsReviewer": "auto_review" })), "t").unwrap());
        assert_eq!(record.approvals_reviewer.as_deref(), Some("auto_review"));
        // Absent (older native) or unknown values keep the stored reviewer.
        assert!(!apply_durable_notification(&mut record, 7, &settings(serde_json::json!({ "model": null })), "t").unwrap());
        assert!(!apply_durable_notification(&mut record, 7, &settings(serde_json::json!({ "approvalsReviewer": "someone" })), "t").unwrap());
        // The legacy alias is stored canonically.
        assert!(!apply_durable_notification(&mut record, 7, &settings(serde_json::json!({ "approvalsReviewer": "guardian_subagent" })), "t").unwrap());
        assert!(apply_durable_notification(&mut record, 7, &settings(serde_json::json!({ "approvalsReviewer": "user" })), "t").unwrap());
        assert_eq!(record.approvals_reviewer.as_deref(), Some("user"));
    }

    #[test]
    fn cutex_workspace_profile_syncs_as_the_workspace_alias() {
        let mut record = record();
        record.sandbox_mode = Some("workspace-write".into());
        record.sandbox_network_access = Some(true);
        let notification = RpcNotification {
            method: "thread/settings/updated".to_string(),
            params: Some(serde_json::json!({ "threadSettings": {
                "activePermissionProfile": { "id": cutex::launch::stock::CUTEX_WORKSPACE_PERMISSION_PROFILE }
            }})),
            raw: serde_json::json!({}),
        };
        apply_durable_notification(&mut record, 7, &notification, "t").unwrap();
        assert_eq!(record.permission_defaults.as_deref(), Some(":workspace"));
        assert_eq!(record.sandbox_network_access, Some(true));
        assert_eq!(
            cutex::runtime::args::effective_runtime_permission_defaults(&record).0.as_deref(),
            Some("workspace-write")
        );
    }

    #[test]
    fn running_sandbox_policy_change_recomputes_the_alias() {
        // Linux host, session cutex.01a1141b (r208): the desktop switched a workspace thread
        // to full access; only sandbox_mode followed, bricking the next online.
        let mut record = record();
        record.permission_defaults = Some("workspace-write".into());
        record.approval_policy = Some("on-request".into());
        let notification = RpcNotification {
            method: "thread/settings/updated".to_string(),
            params: Some(serde_json::json!({ "threadSettings": {
                "approvalPolicy": "never",
                "activePermissionProfile": null,
                "sandboxPolicy": { "type": "dangerFullAccess" }
            }})),
            raw: serde_json::json!({}),
        };
        assert!(apply_durable_notification(&mut record, 7, &notification, "t").unwrap());
        assert_eq!(record.sandbox_mode.as_deref(), Some("danger-full-access"));
        assert_eq!(record.approval_policy.as_deref(), Some("never"));
        assert_eq!(record.permission_defaults.as_deref(), Some("danger-full-access"));
        assert_eq!(cutex::runtime::args::inconsistent_permission_alias_repair(&record), None);

        // Back to workspace: alias and sandbox move together again.
        let notification = RpcNotification {
            method: "thread/settings/updated".to_string(),
            params: Some(serde_json::json!({ "threadSettings": {
                "approvalPolicy": "on-request",
                "sandboxPolicy": { "type": "workspaceWrite", "networkAccess": true }
            }})),
            raw: serde_json::json!({}),
        };
        assert!(apply_durable_notification(&mut record, 7, &notification, "t").unwrap());
        assert_eq!(record.sandbox_mode.as_deref(), Some("workspace-write"));
        assert_eq!(record.permission_defaults.as_deref(), Some("workspace-write"));
    }

    fn settings_updated(value: serde_json::Value) -> RpcNotification {
        RpcNotification {
            method: "thread/settings/updated".to_string(),
            params: Some(serde_json::json!({ "threadSettings": value })),
            raw: serde_json::json!({}),
        }
    }

    /// The desktop's 「自动」 on a running thread: legacy sandbox policy, no
    /// named permission profile (the native reports it as null).
    fn auto_switch(network: bool, roots: serde_json::Value) -> RpcNotification {
        settings_updated(serde_json::json!({
            "approvalPolicy": "on-request",
            "approvalsReviewer": "auto_review",
            "activePermissionProfile": null,
            "sandboxPolicy": { "type": "workspaceWrite", "networkAccess": network, "writableRoots": roots,
                "excludeTmpdirEnvVar": false, "excludeSlashTmp": false }
        }))
    }

    fn resume_extras(record: &cutex::session::model::CutexSessionRecord) -> (Option<String>, Option<String>, Option<serde_json::Value>, Option<String>) {
        let mut record = record.clone();
        record.codex_session_id.get_or_insert_with(|| "018f0000-0000-7000-8000-000000000001".to_string());
        let params = cutex::app_server::runtime::thread_resume_params_for_session(&record).unwrap();
        (
            params.permissions,
            params.sandbox,
            params.config.and_then(|config| config.get("sandbox_workspace_write").cloned()),
            params.approvals_reviewer,
        )
    }

    #[test]
    fn verbatim_runtime_cwd_does_not_replace_the_managed_cwd() {
        // On Windows a Claude agent's record cwd was the canonicalize()
        // output `\\?\F:\cutex-test\r244`, its managed cwd the Project root
        // `F:\cutex-test\r244`. The adapter echoes its launch cwd in every
        // settings update; storing that text made `member_runtime restart`
        // fail with `runtime recovery mismatch: cwd`.
        let mut record = cutex::session::model::CutexSessionRecord::new_at(
            "cutex.verbatim".to_string(),
            Some("thread-verbatim".to_string()),
            "winbox".to_string(),
            r"\\?\F:\cutex-test\r244".to_string(),
            None,
            "2026-10-18T00:00:00Z".to_string(),
        )
        .unwrap();
        record.runtime_generation = 7;
        record.managed_cwd = Some(r"F:\cutex-test\r244".to_string());
        let echo = settings_updated(serde_json::json!({ "cwd": r"\\?\F:\cutex-test\r244" }));
        assert!(!apply_durable_notification(&mut record, 7, &echo, "t").unwrap());
        assert_eq!(record.managed_cwd.as_deref(), Some(r"F:\cutex-test\r244"));
        // A real move is kept, without the verbatim prefix.
        let moved = settings_updated(serde_json::json!({ "cwd": r"\\?\G:\elsewhere" }));
        assert!(apply_durable_notification(&mut record, 7, &moved, "t").unwrap());
        assert_eq!(record.managed_cwd.as_deref(), Some(r"G:\elsewhere"));
        // An unpinned record still pins its current spelling.
        let mut unpinned = record.clone();
        unpinned.managed_cwd = None;
        assert!(apply_durable_notification(&mut unpinned, 7, &echo, "t").unwrap());
        assert_eq!(unpinned.managed_cwd.as_deref(), Some(r"\\?\F:\cutex-test\r244"));
    }

    #[test]
    fn running_auto_switch_persists_network_roots_and_reviewer_for_resume_old_home() {
        // Old home: agents started with full access and no network
        // (fresh-agent migration kept the old defaults).
        let mut record = record();
        record.managed_cwd = Some("/tmp/state-sync".into());
        record.permission_defaults = Some("danger-full-access".into());
        record.approval_policy = Some("never".into());
        let switch = auto_switch(true, serde_json::json!(["/tmp/state-sync", "/extra/a", "/extra/a", "", "/extra/b"]));
        assert!(apply_durable_notification(&mut record, 7, &switch, "t").unwrap());
        assert_eq!(record.sandbox_network_access, Some(true));
        assert_eq!(record.sandbox_writable_roots, vec!["/extra/a".to_string(), "/extra/b".to_string()]);
        assert_eq!(record.approvals_reviewer.as_deref(), Some("auto_review"));
        assert_eq!(record.permission_defaults.as_deref(), Some("workspace-write"));
        let (permissions, sandbox, extras, reviewer) = resume_extras(&record);
        assert_eq!(permissions, None);
        assert_eq!(sandbox.as_deref(), Some("workspace-write"));
        assert_eq!(extras, Some(serde_json::json!({"network_access": true, "writable_roots": ["/extra/a", "/extra/b"]})));
        assert_eq!(reviewer.as_deref(), Some("auto_review"));
        // The same notification again is no durable change.
        assert!(!apply_durable_notification(&mut record, 7, &switch, "t").unwrap());
        // A thread start (fresh native thread for the record) uses the same values.
        let mut fresh = record.clone();
        fresh.codex_session_id = None;
        let start = cutex::app_server::runtime::thread_start_params_for_session(&fresh, None).unwrap();
        assert_eq!(start.sandbox.as_deref(), Some("workspace-write"));
        assert_eq!(start.config.unwrap()["sandbox_workspace_write"]["network_access"], true);
    }

    #[test]
    fn running_switch_turning_extras_off_survives_resume_new_home() {
        // New home: workspace-write with network on by default.
        let mut record = record();
        record.permission_defaults = Some("workspace-write".into());
        record.approval_policy = Some("on-request".into());
        record.sandbox_network_access = Some(true);
        record.sandbox_writable_roots = vec!["/old".into()];
        assert_eq!(resume_extras(&record).2.unwrap()["network_access"], true);
        assert!(apply_durable_notification(&mut record, 7, &auto_switch(false, serde_json::json!([])), "t").unwrap());
        assert_eq!(record.sandbox_network_access, Some(false));
        assert!(record.sandbox_writable_roots.is_empty());
        let (permissions, sandbox, extras, _) = resume_extras(&record);
        assert_eq!(permissions.as_deref(), Some(":workspace"));
        assert_eq!(sandbox, None);
        assert_eq!(extras, None);
        // A policy without the keys (older native) keeps what the record has.
        record.sandbox_writable_roots = vec!["/kept".into()];
        let partial = settings_updated(serde_json::json!({ "sandboxPolicy": { "type": "workspaceWrite" } }));
        apply_durable_notification(&mut record, 7, &partial, "t").unwrap();
        assert_eq!(record.sandbox_writable_roots, vec!["/kept".to_string()]);
        assert_eq!(record.sandbox_network_access, Some(false));
    }

    #[test]
    fn quick_chat_records_sync_extras_like_agents() {
        let mut record = record();
        record.explicit_launch = Some(
            serde_json::from_value(serde_json::json!({
                "version": 4, "native_id": "0189aaaa-0000-7000-8000-000000000001",
                "native_home": "/tmp/native", "bundle_manifest": "/tmp/manifest.json",
                "bundle_sha256": "e".repeat(64),
            }))
            .unwrap(),
        );
        assert!(record.is_owned_session());
        record.permission_defaults = Some("read-only".into());
        record.approval_policy = Some("on-request".into());
        assert!(apply_durable_notification(&mut record, 7, &auto_switch(true, serde_json::json!(["/notes"])), "t").unwrap());
        let (_, sandbox, extras, reviewer) = resume_extras(&record);
        assert_eq!(sandbox.as_deref(), Some("workspace-write"));
        assert_eq!(extras, Some(serde_json::json!({"network_access": true, "writable_roots": ["/notes"]})));
        assert_eq!(reviewer.as_deref(), Some("auto_review"));
    }

    #[test]
    fn named_permission_profile_keeps_the_record_extras() {
        // cutex's own extras profile (or any named profile) defines network /
        // roots itself; the legacy projection next to it is not copied back.
        for id in [cutex::launch::stock::CUTEX_WORKSPACE_PERMISSION_PROFILE, "team-profile"] {
            let mut record = record();
            record.permission_defaults = Some("workspace-write".into());
            record.sandbox_network_access = Some(true);
            record.sandbox_writable_roots = vec!["/extra".into()];
            let notification = settings_updated(serde_json::json!({
                "activePermissionProfile": { "id": id },
                "sandboxPolicy": { "type": "workspaceWrite", "networkAccess": false, "writableRoots": [] }
            }));
            apply_durable_notification(&mut record, 7, &notification, "t").unwrap();
            assert_eq!(record.sandbox_network_access, Some(true), "{id}");
            assert_eq!(record.sandbox_writable_roots, vec!["/extra".to_string()], "{id}");
        }
    }

    #[test]
    fn non_workspace_policies_leave_the_extras() {
        let mut record = record();
        record.sandbox_network_access = Some(true);
        record.sandbox_writable_roots = vec!["/extra".into()];
        let full = settings_updated(serde_json::json!({ "approvalPolicy": "never", "activePermissionProfile": null,
            "sandboxPolicy": { "type": "dangerFullAccess" } }));
        assert!(apply_durable_notification(&mut record, 7, &full, "t").unwrap());
        let read_only = settings_updated(serde_json::json!({ "sandboxPolicy": { "type": "readOnly", "networkAccess": false } }));
        apply_durable_notification(&mut record, 7, &read_only, "t").unwrap();
        assert_eq!(record.sandbox_network_access, Some(true));
        assert_eq!(record.sandbox_writable_roots, vec!["/extra".to_string()]);
    }

    #[test]
    fn sandbox_policy_names_remain_stable() {
        assert_eq!(camel_to_kebab("dangerFullAccess"), "danger-full-access");
        assert_eq!(camel_to_kebab("workspaceWrite"), "workspace-write");
    }

    #[test]
    fn durable_notification_bumps_revision_only_for_current_effective_change() {
        let mut record = record();
        let notification = thread_name_notification("renamed");

        assert!(
            !apply_durable_notification(&mut record, 6, &notification, "2026-08-10T00:01:00Z")
                .expect("stale notification")
        );
        assert_eq!(record.thread_name, None);
        assert_eq!(record.durable_revision(), 1);

        assert!(
            apply_durable_notification(&mut record, 7, &notification, "2026-08-10T00:02:00Z")
                .expect("current notification")
        );
        assert_eq!(record.thread_name.as_deref(), Some("renamed"));
        assert_eq!(record.durable_revision(), 2);
        assert_eq!(record.updated_at, "2026-08-10T00:02:00Z");

        assert!(
            !apply_durable_notification(&mut record, 7, &notification, "2026-08-10T00:03:00Z")
                .expect("duplicate notification")
        );
        assert_eq!(record.durable_revision(), 2);
        assert_eq!(record.updated_at, "2026-08-10T00:02:00Z");
    }

    #[test]
    fn retired_session_ignores_current_generation_notification() {
        let mut record = record();
        record.archive_state = cutex::session::model::CutexSessionArchiveState::Retired;
        record.retired_at = Some("2026-08-10T00:01:00Z".to_string());
        let original = record.clone();

        assert!(!apply_durable_notification(
            &mut record,
            7,
            &thread_name_notification("must-not-apply"),
            "2026-08-10T00:02:00Z"
        )
        .expect("retired notification"));
        assert_eq!(record, original);
    }
}
