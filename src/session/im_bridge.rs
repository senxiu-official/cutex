//! Bridges durable `cutex_session` records with the legacy IM registry shape.

use crate::agent_bus::identity::default_agent_group_for;
use crate::agent_bus::model::AgentRegistrationClass;
use crate::im::registry::load_im_registry;
use crate::im::registry::save_im_registry;
use crate::im::registry::CodingSessionRegistration;
use crate::im::registry::ImRegistry;
use crate::session::identity::normalize_codex_session_id;
use crate::session::model::CutexSessionRecord;
use crate::session::model::CutexSessionStore;
use crate::session::service::apply_managed_session_defaults;
use crate::session::service::cutex_session_key_for_codex_session;
use crate::session::service::cutex_session_key_for_codex_session_including_retired;
use crate::session::service::cutex_session_launch_cwd;
use crate::session::store::load_cutex_session_store;
use crate::session::store::save_cutex_session_store;

pub fn reconcile_cutex_session_store_from_im_registration(
    store: &mut CutexSessionStore,
    entry: &CodingSessionRegistration,
    timestamp: &str,
) -> anyhow::Result<bool> {
    let codex_session_id = normalize_codex_session_id(&entry.session_id)?;
    if let Some(key) =
        cutex_session_key_for_codex_session_including_retired(store, &codex_session_id)
    {
        if store
            .sessions
            .get(&key)
            .is_some_and(|record| crate::agent_management::agent_execution_blocked(&record))
        {
            return Ok(false);
        }
    }
    let session_key = cutex_session_key_for_codex_session(store, &codex_session_id);
    let created = !store.sessions.contains_key(&session_key);
    if created {
        store.sessions.insert(
            session_key.clone(),
            CutexSessionRecord::new_at(
                session_key.clone(),
                Some(codex_session_id.clone()),
                entry.host_id.clone(),
                entry.cwd.clone(),
                None,
                timestamp.to_string(),
            )?,
        );
    }
    let record = store.sessions.get_mut(&session_key).ok_or_else(|| {
        anyhow::anyhow!("cutex session disappeared during IM reconciliation: {session_key}")
    })?;
    let first_management =
        created || record.registration_class != AgentRegistrationClass::Persistent;
    let mut changed = created;
    changed |= replace_if_changed(&mut record.codex_session_id, Some(codex_session_id));
    changed |= replace_if_changed(
        &mut record.display_name_hint,
        Some(entry.display_name.clone()),
    );
    changed |= replace_if_changed(&mut record.host_id, entry.host_id.clone());
    changed |= replace_if_changed(&mut record.cwd, entry.cwd.clone());
    changed |= replace_if_changed(&mut record.agent_groups, entry.groups.clone());
    changed |= replace_if_changed(&mut record.registration_class, entry.registration_class);
    changed |= replace_if_changed(&mut record.exposed_to_backend, entry.visible);
    if record.registration_class == AgentRegistrationClass::Persistent {
        if first_management {
            record.runtime_backend =
                crate::session::service::default_managed_session_runtime_backend();
        }
        let before = record.clone();
        apply_managed_session_defaults(record, None, None, Vec::new(), entry.visible, false);
        // The managed defaults set the flag; the IM registry entry still owns
        // it here (a pure IM-mirror bit, no owner-visibility effect).
        record.exposed_to_backend = entry.visible;
        changed |= *record != before;
    }
    if changed {
        if !created {
            record.bump_durable_revision()?;
        }
        record.updated_at = timestamp.to_string();
    }
    Ok(changed)
}

pub fn reconcile_cutex_session_from_im_registration(
    entry: &CodingSessionRegistration,
) -> anyhow::Result<()> {
    let timestamp = chrono::Utc::now().to_rfc3339();
    let mut store = load_cutex_session_store()?;
    if reconcile_cutex_session_store_from_im_registration(&mut store, entry, &timestamp)? {
        save_cutex_session_store(&store)?;
    }
    Ok(())
}

fn replace_if_changed<T: PartialEq>(slot: &mut T, value: T) -> bool {
    if *slot == value {
        false
    } else {
        *slot = value;
        true
    }
}

pub fn im_registry_from_cutex_session_store(
    store: &CutexSessionStore,
    legacy_registry: &ImRegistry,
) -> ImRegistry {
    let mut sessions = std::collections::HashMap::new();
    let retired_session_ids = store
        .sessions
        .values()
        .filter(|record| crate::agent_management::agent_execution_blocked(&record))
        .filter_map(|record| record.codex_session_id.clone())
        .collect::<std::collections::HashSet<_>>();
    for record in store.sessions.values() {
        if let Some(entry) = coding_registration_from_cutex_session_record(record) {
            sessions.insert(entry.session_id.clone(), entry);
        }
    }
    for (session_id, entry) in &legacy_registry.sessions {
        if retired_session_ids.contains(session_id) {
            continue;
        }
        sessions
            .entry(session_id.clone())
            .or_insert_with(|| entry.clone());
    }
    ImRegistry { sessions }
}

pub fn coding_registration_from_cutex_session_record(
    record: &CutexSessionRecord,
) -> Option<CodingSessionRegistration> {
    if crate::agent_management::agent_execution_blocked(&record) {
        return None;
    }
    let session_id = record.codex_session_id.clone()?;
    let display_name = record
        .display_name_hint
        .clone()
        .or_else(|| record.thread_name.clone())
        .unwrap_or_else(|| session_id.clone());
    let groups = if record.agent_groups.is_empty() {
        vec![default_agent_group_for(
            None,
            cutex_session_launch_cwd(record),
        )]
    } else {
        record.agent_groups.clone()
    };
    Some(CodingSessionRegistration {
        session_id,
        display_name,
        host_id: record.host_id.clone(),
        cwd: record.cwd.clone(),
        profile: record.profile.clone(),
        groups,
        registration_class: record.registration_class,
        visible: record.exposed_to_backend,
        created_at: record.created_at.clone(),
        updated_at: record.updated_at.clone(),
        last_runtime_agent_id: record
            .current_runtime_agent_id
            .clone()
            .or_else(|| record.last_runtime_agent_id.clone()),
    })
}

pub fn persist_cutex_session_store_and_im_record(
    store: &CutexSessionStore,
    key: &str,
) -> anyhow::Result<()> {
    save_cutex_session_store(store)?;
    persist_cutex_session_im_record(store, key)
}

/// Drop the IM projection of a deleted session (keyed by its native ID).
pub fn remove_cutex_session_im_record(native_session_id: &str) -> anyhow::Result<()> {
    let mut registry = load_im_registry()?;
    if registry.sessions.remove(native_session_id).is_some() {
        save_im_registry(&registry)?;
    }
    Ok(())
}

/// Update the IM projection after the durable record has already committed.
pub fn persist_cutex_session_im_record(store: &CutexSessionStore, key: &str) -> anyhow::Result<()> {
    let Some(record) = store.sessions.get(key) else {
        return Ok(());
    };
    let Some(session_id) = record.codex_session_id.as_deref() else {
        return Ok(());
    };
    let mut registry = load_im_registry()?;
    if let Some(entry) = coding_registration_from_cutex_session_record(record) {
        registry.sessions.insert(entry.session_id.clone(), entry);
    } else {
        registry.sessions.remove(session_id);
    }
    save_im_registry(&registry)
}
