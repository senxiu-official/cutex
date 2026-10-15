//! Live runtime-agent reconciliation for durable `cutex_session` records.

use crate::agent_bus::model::AgentBusAgent;
use crate::agent_bus::model::AgentRegistrationClass;
use crate::session::identity::normalize_codex_session_id;
use crate::session::metadata::cutex_session_key_for_codex_session;
use crate::session::metadata::cutex_session_key_for_codex_session_including_retired;
use crate::session::model::CutexSessionReconcileEvent;
use crate::session::model::CutexSessionReconcileOutcome;
use crate::session::model::CutexSessionRecord;
use crate::session::model::CutexSessionRuntimeBackend;
use crate::session::model::CutexSessionStore;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CutexSessionRegistrationReconcile {
    pub outcome: Option<CutexSessionReconcileOutcome>,
    pub store_fence_required: bool,
}

pub fn reconcile_cutex_session_store_for_registration(
    store: &mut CutexSessionStore,
    agent: &AgentBusAgent,
    host_id: &str,
    timestamp: &str,
) -> anyhow::Result<CutexSessionRegistrationReconcile> {
    let target = registration_store_target(store, agent)?;
    let store_fence_required = !matches!(target, RegistrationStoreTarget::Unmanaged);
    let outcome = match target {
        RegistrationStoreTarget::NativeSession => {
            reconcile_cutex_session_store_from_agent(store, agent, host_id, timestamp)?
        }
        RegistrationStoreTarget::RuntimeIdentity(key) => {
            let record = store.sessions.get_mut(&key).ok_or_else(|| {
                anyhow::anyhow!("registration session disappeared during reconciliation: {key}")
            })?;
            if record.current_runtime_agent_id.as_deref() != Some(agent.id.as_str()) {
                if let Some(previous) = record.current_runtime_agent_id.replace(agent.id.clone()) {
                    record.last_runtime_agent_id = Some(previous);
                }
                record.runtime_generation = record.runtime_generation.saturating_add(1);
            }
            record.last_seen_at = Some(timestamp.to_string());
            record.updated_at = timestamp.to_string();
            None
        }
        RegistrationStoreTarget::Unmanaged => None,
    };
    debug_assert!(store_fence_required || outcome.is_none());
    Ok(CutexSessionRegistrationReconcile {
        outcome,
        store_fence_required,
    })
}

/// How old a record's `last_seen_at` may get before a registration refresh
/// that changes nothing else is written back. Agents refresh their bus
/// registration every 30 s; persisting each refresh rewrote the whole session
/// store (tens of MB with many sessions) every few seconds, and every rewrite
/// made each reader re-parse it. `last_seen_at` only orders the session list.
pub const REGISTRATION_HEARTBEAT_PERSIST_INTERVAL_SECS: i64 = 30 * 60;

/// True when registering `agent` against `store` would only refresh the
/// heartbeat fields (`last_seen_at`, `updated_at`) of one existing record
/// whose `last_seen_at` is younger than
/// [`REGISTRATION_HEARTBEAT_PERSIST_INTERVAL_SECS`], so the caller may skip
/// the store write. Read-only: the reconciliation runs on a copy of just the
/// records it can touch. Any doubt (errors, new or rebound records, durable
/// changes, unparsable timestamps) answers false and the caller takes the
/// full write path.
pub fn registration_is_recent_heartbeat(
    store: &CutexSessionStore,
    agent: &AgentBusAgent,
    host_id: &str,
    timestamp: &str,
) -> bool {
    let codex_session_id = match agent
        .session_id
        .as_deref()
        .map(normalize_codex_session_id)
        .transpose()
    {
        Ok(id) => id,
        Err(_) => return false,
    };
    let default_key = codex_session_id
        .as_deref()
        .map(crate::session::identity::default_cutex_session_id_for_codex_session);
    // Every record the registration reconciliation reads or writes: the
    // native session's records (any archive state), the default key it would
    // create, and every record bound to this runtime identity.
    let mut relevant = CutexSessionStore::default();
    for (key, record) in &store.sessions {
        let touches = (codex_session_id.is_some()
            && record.codex_session_id.as_deref() == codex_session_id.as_deref())
            || default_key.as_deref() == Some(key.as_str())
            || record.current_runtime_agent_id.as_deref() == Some(agent.id.as_str())
            || record.last_runtime_agent_id.as_deref() == Some(agent.id.as_str());
        if touches {
            relevant.sessions.insert(key.clone(), record.clone());
        }
    }
    let before = relevant.sessions.clone();
    match reconcile_cutex_session_store_for_registration(&mut relevant, agent, host_id, timestamp)
    {
        Ok(reconciled) if reconciled.store_fence_required => {
            if reconciled
                .outcome
                .as_ref()
                .is_some_and(|outcome| !outcome.events.is_empty())
            {
                return false;
            }
        }
        _ => return false,
    }
    if before.len() != relevant.sessions.len() {
        return false;
    }
    let mut refreshed = None;
    for (key, after) in &relevant.sessions {
        let Some(previous) = before.get(key) else {
            return false;
        };
        if previous == after {
            continue;
        }
        let mut heartbeat_reverted = after.clone();
        heartbeat_reverted.last_seen_at = previous.last_seen_at.clone();
        heartbeat_reverted.updated_at = previous.updated_at.clone();
        if &heartbeat_reverted != previous || refreshed.replace(previous).is_some() {
            return false;
        }
    }
    let Some(previous) = refreshed else {
        return true;
    };
    let (Some(last_seen), Ok(now)) = (
        previous
            .last_seen_at
            .as_deref()
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok()),
        chrono::DateTime::parse_from_rfc3339(timestamp),
    ) else {
        return false;
    };
    let age = now.signed_duration_since(last_seen).num_seconds();
    (0..REGISTRATION_HEARTBEAT_PERSIST_INTERVAL_SECS).contains(&age)
}

enum RegistrationStoreTarget {
    NativeSession,
    RuntimeIdentity(String),
    Unmanaged,
}

fn registration_store_target(
    store: &CutexSessionStore,
    agent: &AgentBusAgent,
) -> anyhow::Result<RegistrationStoreTarget> {
    let codex_session_id = agent
        .session_id
        .as_deref()
        .map(normalize_codex_session_id)
        .transpose()?;

    if let Some(codex_session_id) = codex_session_id.as_deref() {
        if let Some(record) = store.sessions.values().find(|record| {
            record.is_retired() && record.codex_session_id.as_deref() == Some(codex_session_id)
        }) {
            anyhow::bail!(
                "agent registration targets retired cutex session: {}",
                record.cutex_session_id
            );
        }
        return Ok(RegistrationStoreTarget::NativeSession);
    }

    let mut active_runtime_match = None;
    for (key, record) in store.sessions.iter().filter(|(_, record)| {
        record.current_runtime_agent_id.as_deref() == Some(agent.id.as_str())
            || record.last_runtime_agent_id.as_deref() == Some(agent.id.as_str())
    }) {
        if record.is_retired() {
            anyhow::bail!(
                "agent registration targets retired cutex session: {}",
                record.cutex_session_id
            );
        }
        if active_runtime_match.replace(key.clone()).is_some() {
            anyhow::bail!(
                "agent registration runtime identity is ambiguous across cutex sessions: {}",
                agent.id
            );
        }
    }
    Ok(active_runtime_match
        .map(RegistrationStoreTarget::RuntimeIdentity)
        .unwrap_or(RegistrationStoreTarget::Unmanaged))
}

pub fn reconcile_cutex_session_store_from_agent(
    store: &mut CutexSessionStore,
    agent: &AgentBusAgent,
    host_id: &str,
    timestamp: &str,
) -> anyhow::Result<Option<CutexSessionReconcileOutcome>> {
    let Some(codex_session_id) = agent.session_id.as_deref() else {
        return Ok(None);
    };
    let codex_session_id = normalize_codex_session_id(codex_session_id)?;
    if let Some(key) =
        cutex_session_key_for_codex_session_including_retired(store, &codex_session_id)
    {
        if store
            .sessions
            .get(&key)
            .is_some_and(|record| record.is_retired())
        {
            return Ok(None);
        }
    }
    let session_key = cutex_session_key_for_codex_session(store, &codex_session_id);
    let mut events = Vec::new();

    let stale_keys = store
        .sessions
        .iter()
        .filter_map(|(key, record)| {
            (key != &session_key
                && record.is_active()
                && record.current_runtime_agent_id.as_deref() == Some(agent.id.as_str()))
            .then(|| key.clone())
        })
        .collect::<Vec<_>>();

    for stale_key in stale_keys {
        if let Some(record) = store.sessions.get_mut(&stale_key) {
            let previous_runtime_agent_id = record.current_runtime_agent_id.take();
            record.last_runtime_agent_id = previous_runtime_agent_id.clone();
            record.runtime_generation = record.runtime_generation.saturating_add(1);
            record.updated_at = timestamp.to_string();
            events.push(CutexSessionReconcileEvent {
                event_type: "cutex_session_rebound",
                summary: format!(
                    "Runtime endpoint {} moved from {} to {}",
                    agent.id,
                    record
                        .codex_session_id
                        .as_deref()
                        .unwrap_or(stale_key.as_str()),
                    codex_session_id
                ),
                previous_runtime_agent_id,
                runtime_agent_id: Some(agent.id.clone()),
                previous_cutex_session_id: Some(stale_key),
            });
        }
    }

    let created = !store.sessions.contains_key(&session_key);
    if created {
        let mut record = CutexSessionRecord::new_at(
            session_key.clone(),
            Some(codex_session_id.clone()),
            host_id.to_string(),
            agent.cwd.clone(),
            None,
            timestamp.to_string(),
        )?;
        record.runtime_generation = 1;
        store.sessions.insert(session_key.clone(), record);
    }

    let record = store.sessions.get_mut(&session_key).ok_or_else(|| {
        anyhow::anyhow!("cutex session disappeared during reconciliation: {session_key}")
    })?;
    let owned_session = record.is_owned_session();
    let previous_runtime_agent_id = record.current_runtime_agent_id.clone();
    let endpoint_changed = previous_runtime_agent_id.as_deref() != Some(agent.id.as_str());
    if endpoint_changed {
        record.last_runtime_agent_id = previous_runtime_agent_id.clone();
        record.current_runtime_agent_id = Some(agent.id.clone());
        if !created {
            record.runtime_generation = record.runtime_generation.saturating_add(1);
        }
        let first_runtime_endpoint = previous_runtime_agent_id.is_none();
        events.push(CutexSessionReconcileEvent {
            event_type: if created || first_runtime_endpoint {
                "runtime_endpoint_registered"
            } else {
                "runtime_endpoint_changed"
            },
            summary: if created || first_runtime_endpoint {
                format!(
                    "Runtime endpoint {} registered for session {}",
                    agent.id, codex_session_id
                )
            } else {
                format!(
                    "Runtime endpoint for session {} changed to {}",
                    codex_session_id, agent.id
                )
            },
            previous_runtime_agent_id,
            runtime_agent_id: Some(agent.id.clone()),
            previous_cutex_session_id: None,
        });
    }

    let mut durable_changed = false;
    durable_changed |=
        replace_if_changed(&mut record.codex_session_id, Some(codex_session_id.clone()));
    if let Some(thread_name) = agent.thread_name.clone() {
        durable_changed |= replace_if_changed(&mut record.thread_name, Some(thread_name));
    }
    // A persistent session's display-name hint is its stable managed identity.
    // The runtime thread name is a mutable conversation title (often generated
    // from the first user message), so it must not replace an existing managed
    // name. Unmanaged sessions still follow their runtime title, and legacy
    // persistent records without a display hint may be initialized here.
    // Owned sessions (quick chats) have no managed identity either: once the
    // native thread has a title, their hint follows it (the creation-time
    // "Session <id>" placeholder is replaced).
    if owned_session && agent.thread_name.is_some() {
        durable_changed |=
            replace_if_changed(&mut record.display_name_hint, agent.thread_name.clone());
    } else if (record.registration_class != AgentRegistrationClass::Persistent && !owned_session)
        || record.display_name_hint.is_none()
    {
        durable_changed |= replace_if_changed(
            &mut record.display_name_hint,
            agent
                .thread_name
                .clone()
                .or_else(|| agent.base_name.clone())
                .or_else(|| Some(agent.name.clone())),
        );
    }
    durable_changed |= replace_if_changed(&mut record.host_id, host_id.to_string());
    if record.managed_cwd.is_none() {
        durable_changed |= replace_if_changed(
            &mut record.cwd,
            crate::session::metadata::normalize_session_cwd(&agent.cwd),
        );
    }
    if !owned_session { durable_changed |= replace_if_changed(&mut record.agent_enabled, true); }
    durable_changed |= replace_if_changed(&mut record.agent_groups, agent.groups.clone());
    let registration_class =
        reconcile_registration_class(record.registration_class, agent.registration_class);
    durable_changed |= replace_if_changed(&mut record.registration_class, registration_class);
    if durable_changed && !created {
        record.bump_durable_revision()?;
    }
    record.last_seen_at = Some(timestamp.to_string());
    record.updated_at = timestamp.to_string();

    Ok(Some(CutexSessionReconcileOutcome {
        cutex_session_id: session_key,
        codex_session_id,
        events,
    }))
}

fn replace_if_changed<T: PartialEq>(slot: &mut T, value: T) -> bool {
    if *slot == value {
        false
    } else {
        *slot = value;
        true
    }
}

pub fn apply_session_online_runtime_observation(
    store: &mut CutexSessionStore,
    key: &str,
    live_agent: Option<&AgentBusAgent>,
    alden_session_name: Option<&str>,
    backend: CutexSessionRuntimeBackend,
    pid: u32,
    host_id: &str,
    timestamp: &str,
) -> anyhow::Result<Option<CutexSessionReconcileOutcome>> {
    let reconcile_outcome = if let Some(agent) = live_agent {
        reconcile_cutex_session_store_from_agent(store, agent, host_id, timestamp)?
    } else {
        None
    };
    let record = store.sessions.get_mut(key).ok_or_else(|| {
        anyhow::anyhow!("cutex session disappeared while recording runtime: {key}")
    })?;
    if record.is_retired() {
        anyhow::bail!("cutex session is retired: {key}");
    }
    record.runtime_backend = backend;
    record.alden_session_name = alden_session_name.map(str::to_string);
    record.alden_pid = (backend == CutexSessionRuntimeBackend::CuteAlden).then_some(pid);
    record.runtime_pid = live_agent
        .map(|agent| agent.pid)
        .or((backend == CutexSessionRuntimeBackend::Host).then_some(pid))
        .or(record.runtime_pid);
    record.updated_at = timestamp.to_string();
    Ok(reconcile_outcome)
}

pub fn clear_cutex_session_runtime_record(
    store: &mut CutexSessionStore,
    key: &str,
    preserve_alden_name: bool,
) -> anyhow::Result<()> {
    let Some(record) = store.sessions.get_mut(key) else {
        return Ok(());
    };
    clear_runtime_fields(record, preserve_alden_name);
    Ok(())
}

/// [`clear_cutex_session_runtime_record`] on one record (row-level writers).
pub fn clear_runtime_fields(record: &mut CutexSessionRecord, preserve_alden_name: bool) {
    if !preserve_alden_name {
        record.alden_session_name = None;
    }
    record.pending_launch_id = None;
    record.app_server_launch_claim_id = None;
    record.alden_pid = None;
    record.runtime_pid = None;
    record.app_server_runtime = None;
    record.current_runtime_agent_id = None;
    record.updated_at = chrono::Utc::now().to_rfc3339();
}

fn reconcile_registration_class(
    existing: AgentRegistrationClass,
    observed: AgentRegistrationClass,
) -> AgentRegistrationClass {
    match (existing, observed) {
        (AgentRegistrationClass::Persistent, _) => AgentRegistrationClass::Persistent,
        (_, observed) => observed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_agent(session_id: &str, thread_name: &str) -> AgentBusAgent {
        AgentBusAgent {
            id: format!("runtime-{session_id}"),
            name: format!("runtime-{session_id}"),
            base_name: Some("runtime".to_string()),
            thread_name: Some(thread_name.to_string()),
            path_key: None,
            session_id: Some(session_id.to_string()),
            cutex_session_id: None,
            profile: "default".to_string(),
            cwd: "/tmp".to_string(),
            pid: 42,
            host_id: Some("host".to_string()),
            groups: Vec::new(),
            registration_class: AgentRegistrationClass::Persistent,
            last_seen_epoch_secs: 1,
        }
    }

    /// A runtime registered before the cwd migration still reports the
    /// escaped spelling; neither creating nor updating the record stores it.
    #[test]
    fn registration_cwd_is_normalized_on_create_and_update() {
        let mut store = CutexSessionStore::default();
        let mut agent = live_agent("cwd-escaped", "staff-backend-1");
        agent.cwd = r"E:\\Projects (Orchid)\\waveline-backend".to_string();
        let clean = r"E:\Projects (Orchid)\waveline-backend";
        reconcile_cutex_session_store_from_agent(&mut store, &agent, "host", "2026-10-11T00:00:00Z")
            .unwrap();
        let record = store.sessions.values().next().unwrap();
        assert_eq!(record.cwd, clean);
        let revision = record.durable_revision();

        agent.id = "runtime-cwd-escaped-2".to_string();
        reconcile_cutex_session_store_from_agent(&mut store, &agent, "host", "2026-10-11T00:01:00Z")
            .unwrap();
        let record = store.sessions.values().next().unwrap();
        assert_eq!(record.cwd, clean);
        assert_eq!(record.durable_revision(), revision, "no cwd change, no durable bump");
    }

    #[test]
    fn registration_heartbeat_is_skippable_only_while_recent_and_unchanged() {
        let mut store = CutexSessionStore::default();
        let agent = live_agent("heartbeat-skip", "Heartbeat thread");
        assert!(
            !registration_is_recent_heartbeat(&store, &agent, "host", "2026-09-09T00:00:00Z"),
            "first registration creates a record"
        );
        reconcile_cutex_session_store_for_registration(
            &mut store,
            &agent,
            "host",
            "2026-09-09T00:00:00Z",
        )
        .unwrap();
        let persisted = store.clone();

        assert!(registration_is_recent_heartbeat(
            &store,
            &agent,
            "host",
            "2026-09-09T00:29:59Z"
        ));
        assert_eq!(
            store.sessions, persisted.sessions,
            "the check never mutates the store"
        );
        assert!(
            registration_is_recent_heartbeat(&store, &agent, "host", "2026-09-09T00:00:00Z"),
            "a refresh with the same timestamp changes nothing"
        );
        assert!(
            !registration_is_recent_heartbeat(&store, &agent, "host", "2026-09-09T00:30:00Z"),
            "an old last_seen_at is persisted again"
        );
        assert!(
            !registration_is_recent_heartbeat(&store, &agent, "host", "2026-09-08T23:59:00Z"),
            "a clock step backwards is persisted"
        );

        let renamed = live_agent("heartbeat-skip", "Renamed thread");
        let mut renamed_on_other_host = renamed.clone();
        renamed_on_other_host.thread_name = agent.thread_name.clone();
        assert!(!registration_is_recent_heartbeat(
            &store,
            &renamed,
            "host",
            "2026-09-09T00:01:00Z"
        ));
        assert!(
            !registration_is_recent_heartbeat(
                &store,
                &renamed_on_other_host,
                "other-host",
                "2026-09-09T00:01:00Z"
            ),
            "a host change is durable"
        );

        let mut moved = agent.clone();
        moved.id = "runtime-heartbeat-skip-2".to_string();
        assert!(
            !registration_is_recent_heartbeat(&store, &moved, "host", "2026-09-09T00:01:00Z"),
            "an endpoint change must be written and announced"
        );

        let mut unmanaged = agent.clone();
        unmanaged.id = "unmanaged-heartbeat".to_string();
        unmanaged.session_id = None;
        assert!(!registration_is_recent_heartbeat(
            &store,
            &unmanaged,
            "host",
            "2026-09-09T00:01:00Z"
        ));
    }

    #[test]
    fn stock_reconciliation_preserves_launch_requirement_and_formal_identity() {
        let mut store = CutexSessionStore::default();
        let mut record = CutexSessionRecord::new_at(
            "cutex.stock-reconcile".into(),
            Some("stock-reconcile".into()),
            "host".into(),
            "/tmp".into(),
            Some("mutable-profile".into()),
            "2026-09-09T00:00:00Z".into(),
        )
        .unwrap();
        record.registration_class = AgentRegistrationClass::Persistent;
        record.formal_agent_name = Some("Exact formal Agent".into());
        record.explicit_launch = Some(crate::agent_management::ExplicitLaunchContract {
            version: 999,
            migration_action_id: None,
            native_id: "stock-reconcile".into(),
            native_home: "/unavailable".into(),
            bundle_manifest: "/unavailable/bundle".into(),
            bundle_sha256: crate::role_revision::Sha256::new("a".repeat(64)).unwrap(),
        });
        let requirement = record.explicit_launch.clone();
        store
            .sessions
            .insert(record.cutex_session_id.clone(), record);
        reconcile_cutex_session_store_from_agent(
            &mut store,
            &live_agent("stock-reconcile", "Not a formal name"),
            "host",
            "2026-09-09T00:01:00Z",
        )
        .unwrap();
        let current = &store.sessions["cutex.stock-reconcile"];
        assert_eq!(current.explicit_launch, requirement);
        assert_eq!(

            current.formal_agent_name.as_deref(),
            Some("Exact formal Agent")
        );
        assert_eq!(current.profile.as_deref(), Some("mutable-profile"));
    }

    #[test]
    fn owned_session_display_name_follows_native_thread_title() {
        let mut store = CutexSessionStore::default();
        let mut record = CutexSessionRecord::new_at(
            "cutex.owned-title".into(),
            Some("owned-title".into()),
            "host".into(),
            "/tmp".into(),
            None,
            "2026-10-06T00:00:00Z".into(),
        )
        .unwrap();
        record.explicit_launch = Some(crate::agent_management::ExplicitLaunchContract {
            version: 999,
            migration_action_id: None,
            native_id: "owned-title".into(),
            native_home: "/unavailable".into(),
            bundle_manifest: "/unavailable/bundle".into(),
            bundle_sha256: crate::role_revision::Sha256::new("a".repeat(64)).unwrap(),
        });
        record.display_name_hint = Some("Session owned-ti".into());
        assert!(record.is_owned_session());
        store.sessions.insert(record.cutex_session_id.clone(), record);
        reconcile_cutex_session_store_from_agent(
            &mut store,
            &live_agent("owned-title", "Plan a weekend trip"),
            "host",
            "2026-10-06T00:01:00Z",
        )
        .unwrap();
        let record = &store.sessions["cutex.owned-title"];
        assert_eq!(record.display_name_hint.as_deref(), Some("Plan a weekend trip"));
        assert_eq!(record.formal_agent_name, None);
    }

    #[test]
    fn persistent_session_keeps_managed_display_name_when_runtime_title_changes() {
        let mut store = CutexSessionStore::default();
        let mut record = CutexSessionRecord::new_at(
            "cutex.thread-managed-name".to_string(),
            Some("thread-managed-name".to_string()),
            "host".to_string(),
            "/tmp".to_string(),
            None,
            "2026-08-10T00:00:00Z".to_string(),
        )
        .expect("record");
        record.thread_name = Some("managed-worker-r2".to_string());
        record.display_name_hint = Some("managed-worker-r2".to_string());
        record.registration_class = AgentRegistrationClass::Persistent;
        store
            .sessions
            .insert("cutex.thread-managed-name".to_string(), record);

        reconcile_cutex_session_store_from_agent(
            &mut store,
            &live_agent("thread-managed-name", "generated task title"),
            "host",
            "2026-08-10T00:01:00Z",
        )
        .expect("runtime reconciliation");

        let record = store
            .sessions
            .get("cutex.thread-managed-name")
            .expect("managed record");
        assert_eq!(record.thread_name.as_deref(), Some("generated task title"));
        assert_eq!(

            record.display_name_hint.as_deref(),
            Some("managed-worker-r2")
        );
    }

    #[test]
    fn persistent_session_without_display_name_is_initialized_from_runtime() {
        let mut store = CutexSessionStore::default();
        let mut record = CutexSessionRecord::new_at(
            "cutex.thread-missing-name".to_string(),
            Some("thread-missing-name".to_string()),
            "host".to_string(),
            "/tmp".to_string(),
            None,
            "2026-08-10T00:00:00Z".to_string(),
        )
        .expect("record");
        record.registration_class = AgentRegistrationClass::Persistent;
        store
            .sessions
            .insert("cutex.thread-missing-name".to_string(), record);

        reconcile_cutex_session_store_from_agent(
            &mut store,
            &live_agent("thread-missing-name", "runtime title"),
            "host",
            "2026-08-10T00:01:00Z",
        )
        .expect("runtime reconciliation");

        let record = store
            .sessions
            .get("cutex.thread-missing-name")
            .expect("persistent record");
        assert_eq!(record.display_name_hint.as_deref(), Some("runtime title"));
    }

    #[test]
    fn registration_rejects_retired_identity_while_observation_remains_noop() {
        let mut store = CutexSessionStore::default();
        let mut record = CutexSessionRecord::new_at(
            "cutex.retired-registration".to_string(),
            Some("thread-retired-registration".to_string()),
            "host".to_string(),
            "/tmp".to_string(),
            None,
            "2026-08-10T00:00:00Z".to_string(),
        )
        .expect("record");
        record.archive_state = crate::session::model::CutexSessionArchiveState::Retired;
        record.retired_at = Some("2026-08-10T00:01:00Z".to_string());
        store
            .sessions
            .insert("cutex.retired-registration".to_string(), record);
        let agent = AgentBusAgent {
            id: "runtime-retired-registration".to_string(),
            name: "runtime-retired-registration".to_string(),
            base_name: None,
            thread_name: None,
            path_key: None,
            session_id: Some("thread-retired-registration".to_string()),
            cutex_session_id: None,
            profile: "default".to_string(),
            cwd: "/tmp".to_string(),
            pid: 42,
            host_id: Some("host".to_string()),
            groups: Vec::new(),
            registration_class: AgentRegistrationClass::Persistent,
            last_seen_epoch_secs: 1,
        };

        assert!(reconcile_cutex_session_store_from_agent(
            &mut store,
            &agent,
            "host",
            "2026-08-10T00:02:00Z"
        )
        .expect("refresh reconciliation")
        .is_none());
        let error = reconcile_cutex_session_store_for_registration(
            &mut store,
            &agent,
            "host",
            "2026-08-10T00:02:00Z",
        )
        .expect_err("registration must reject retired identity");
        assert!(error.to_string().contains("retired cutex session"));
    }

    #[test]
    fn sessionless_registration_fences_only_matching_durable_runtime_identity() {
        let mut store = CutexSessionStore::default();
        let mut record = CutexSessionRecord::new_at(
            "cutex.sessionless-registration".to_string(),
            Some("thread-sessionless-registration".to_string()),
            "host".to_string(),
            "/tmp".to_string(),
            None,
            "2026-08-10T00:00:00Z".to_string(),
        )
        .expect("record");
        record.last_runtime_agent_id = Some("runtime-sessionless-registration".to_string());
        store
            .sessions
            .insert("cutex.sessionless-registration".to_string(), record);
        let mut agent = AgentBusAgent {
            id: "runtime-sessionless-registration".to_string(),
            name: "runtime-sessionless-registration".to_string(),
            base_name: None,
            thread_name: None,
            path_key: None,
            session_id: None,
            cutex_session_id: None,
            profile: "default".to_string(),
            cwd: "/tmp".to_string(),
            pid: 42,
            host_id: Some("host".to_string()),
            groups: Vec::new(),
            registration_class: AgentRegistrationClass::Persistent,
            last_seen_epoch_secs: 1,
        };

        let matched = reconcile_cutex_session_store_for_registration(
            &mut store,
            &agent,
            "host",
            "2026-08-10T00:02:00Z",
        )
        .expect("matching sessionless registration");
        assert!(matched.store_fence_required);
        assert!(matched.outcome.is_none());
        let claimed = store
            .sessions
            .get("cutex.sessionless-registration")
            .expect("claimed sessionless record");
        assert_eq!(

            claimed.current_runtime_agent_id.as_deref(),
            Some("runtime-sessionless-registration")
        );
        assert_eq!(claimed.runtime_generation, 1);

        agent.id = "unmanaged-sessionless-registration".to_string();
        let unmatched = reconcile_cutex_session_store_for_registration(
            &mut store,
            &agent,
            "host",
            "2026-08-10T00:03:00Z",
        )
        .expect("unmatched sessionless registration");
        assert!(!unmatched.store_fence_required);
        assert!(unmatched.outcome.is_none());
    }
}
