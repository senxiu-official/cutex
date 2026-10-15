//! Authoritative, presentation-only Cutex participant metadata.

use super::commands::ParticipantPresentationMetadata;
use crate::session::model::{CutexSessionRecord, CutexSessionRuntimeBackend, CutexSessionStore};
use crate::session::store::load_cutex_session_store;

const PARTICIPANT_FIELD_LIMIT: usize = 512;

pub trait ParticipantMetadataResolver: Send + Sync + 'static {
    fn resolve(&self, cutex_session_id: &str) -> ParticipantPresentationMetadata;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RegistryParticipantMetadataResolver;

impl ParticipantMetadataResolver for RegistryParticipantMetadataResolver {
    fn resolve(&self, cutex_session_id: &str) -> ParticipantPresentationMetadata {
        load_cutex_session_store()
            .ok()
            .map(|store| participant_metadata_from_store(&store, cutex_session_id))
            .unwrap_or_else(|| canonical_participant(cutex_session_id))
    }
}

pub fn participant_metadata_from_store(
    store: &CutexSessionStore,
    cutex_session_id: &str,
) -> ParticipantPresentationMetadata {
    let Some(record) = store
        .sessions
        .values()
        .find(|record| record.cutex_session_id == cutex_session_id && record.is_active())
    else {
        return canonical_participant(cutex_session_id);
    };
    metadata_from_record(record)
}

fn metadata_from_record(record: &CutexSessionRecord) -> ParticipantPresentationMetadata {
    let display_name = record
        .display_name_hint
        .as_deref()
        .and_then(nonempty_bounded)
        .or_else(|| record.thread_name.as_deref().and_then(nonempty_bounded))
        .unwrap_or_else(|| bounded(&record.cutex_session_id));
    let profile = record
        .app_server_runtime
        .as_ref()
        .and_then(|binding| binding.current_launched_profile())
        .and_then(|name| nonempty_bounded(&name))
        .or_else(|| record.profile.as_deref().and_then(nonempty_bounded));
    ParticipantPresentationMetadata {
        display_name: Some(display_name),
        cutex_session_id: nonempty_bounded(&record.cutex_session_id),
        profile,
        model: record.model_defaults.as_deref().and_then(nonempty_bounded),
        reasoning: record
            .reasoning_defaults
            .as_deref()
            .and_then(nonempty_bounded),
        // The durable session record currently has no authoritative semantic
        // role field. Profiles and groups must never be relabelled as roles.
        role: None,
        runtime_backend: Some(runtime_backend(record.runtime_backend).to_string()),
    }
}

fn canonical_participant(cutex_session_id: &str) -> ParticipantPresentationMetadata {
    let canonical = nonempty_bounded(cutex_session_id);
    ParticipantPresentationMetadata {
        display_name: canonical.clone(),
        cutex_session_id: canonical,
        ..Default::default()
    }
}

fn runtime_backend(backend: CutexSessionRuntimeBackend) -> &'static str {
    match backend {
        CutexSessionRuntimeBackend::Host => "host",
        CutexSessionRuntimeBackend::HostForeground => "host_foreground",
        CutexSessionRuntimeBackend::Docker => "docker",
        CutexSessionRuntimeBackend::CuteAlden => "cute_alden",
        CutexSessionRuntimeBackend::ClaudeSubscription => "claude",
        CutexSessionRuntimeBackend::Future => "future",
    }
}

fn nonempty_bounded(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| bounded(value))
}

fn bounded(value: &str) -> String {
    value.chars().take(PARTICIPANT_FIELD_LIMIT).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::model::CutexSessionArchiveState;

    fn record() -> CutexSessionRecord {
        CutexSessionRecord::new_at(
            "cutex.worker".to_string(),
            Some("native-thread-must-not-be-display-fallback".to_string()),
            "host-1".to_string(),
            "/private/ignored".to_string(),
            Some("orchid".to_string()),
            "2026-08-28T00:00:00Z".to_string(),
        )
        .unwrap()
    }

    #[test]
    fn metadata_uses_only_registry_fields_and_canonical_fallbacks() {
        let mut store = CutexSessionStore::default();
        let mut worker = record();
        worker.thread_name = Some("worker-thread".to_string());
        worker.model_defaults = Some("gpt-5.6-sol".to_string());
        worker.reasoning_defaults = Some("high".to_string());
        store
            .sessions
            .insert("key-does-not-matter".to_string(), worker);

        let metadata = participant_metadata_from_store(&store, "cutex.worker");
        assert_eq!(metadata.display_name.as_deref(), Some("worker-thread"));
        assert_eq!(metadata.cutex_session_id.as_deref(), Some("cutex.worker"));
        assert_eq!(metadata.profile.as_deref(), Some("orchid"));
        assert_eq!(metadata.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(metadata.reasoning.as_deref(), Some("high"));
        assert_eq!(metadata.role, None);
        assert_eq!(metadata.runtime_backend.as_deref(), Some("host"));

        let missing = participant_metadata_from_store(&store, "cutex.missing");
        assert_eq!(missing.display_name.as_deref(), Some("cutex.missing"));
        assert_eq!(missing.cutex_session_id.as_deref(), Some("cutex.missing"));
        assert_eq!(missing.profile, None);
    }

    #[test]
    fn retired_records_do_not_leak_stale_presentation() {
        let mut store = CutexSessionStore::default();
        let mut worker = record();
        worker.display_name_hint = Some("stale-name".to_string());
        worker.archive_state = CutexSessionArchiveState::Retired;
        store.sessions.insert("cutex.worker".to_string(), worker);

        let metadata = participant_metadata_from_store(&store, "cutex.worker");
        assert_eq!(metadata.display_name.as_deref(), Some("cutex.worker"));
        assert_eq!(metadata.profile, None);
        assert_eq!(metadata.model, None);
    }
}
