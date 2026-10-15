//! Maintain profile-name references across durable Cutex state.

use crate::profiles::model::CodezConfig;
use crate::profiles::model::QuickRunState;
use crate::session::model::CutexSessionStore;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ProfileReferenceChanges {
    pub quick_state_changed: bool,
    pub global_config_changed: bool,
    pub session_keys: Vec<String>,
}

pub fn rename_profile_references(
    state: &mut QuickRunState,
    old_name: &str,
    new_name: &str,
) -> bool {
    if old_name == new_name {
        return false;
    }
    let mut changed = false;
    if state.last_global_profile.as_deref() == Some(old_name) {
        state.last_global_profile = Some(new_name.to_string());
        changed = true;
    }

    for value in state.per_directory.values_mut() {
        if value == old_name {
            *value = new_name.to_string();
            changed = true;
        }
    }
    changed
}

pub fn rename_global_profile_references(
    config: &mut CodezConfig,
    old_name: &str,
    new_name: &str,
) -> bool {
    if old_name == new_name {
        return false;
    }
    let mut changed = false;
    for slot in [&mut config.default_profile, &mut config.quick_chat_default_profile] {
        if slot.as_deref() == Some(old_name) {
            *slot = Some(new_name.to_string());
            changed = true;
        }
    }
    if let Some(defaults) = config.new_session_defaults.remove(old_name) {
        config.new_session_defaults.insert(new_name.to_string(), defaults);
        changed = true;
    }
    changed
}

pub fn remove_profile_references(state: &mut QuickRunState, removed_name: &str) -> bool {
    let mut changed = false;
    if state.last_global_profile.as_deref() == Some(removed_name) {
        state.last_global_profile = None;
        changed = true;
    }

    let old_len = state.per_directory.len();
    state.per_directory.retain(|_, value| value != removed_name);
    changed || old_len != state.per_directory.len()
}

pub fn remove_global_profile_references(config: &mut CodezConfig, removed_name: &str) -> bool {
    let mut changed = false;
    for slot in [&mut config.default_profile, &mut config.quick_chat_default_profile] {
        if slot.as_deref() == Some(removed_name) {
            *slot = None;
            changed = true;
        }
    }
    changed |= config.new_session_defaults.remove(removed_name).is_some();
    changed
}

pub fn rename_all_profile_references(
    state: &mut QuickRunState,
    config: &mut CodezConfig,
    sessions: &mut CutexSessionStore,
    old_name: &str,
    new_name: &str,
) -> anyhow::Result<ProfileReferenceChanges> {
    let quick_state_changed = rename_profile_references(state, old_name, new_name);
    let global_config_changed = rename_global_profile_references(config, old_name, new_name);
    let session_keys = update_session_profile_references(sessions, old_name, Some(new_name))?;
    Ok(ProfileReferenceChanges {
        quick_state_changed,
        global_config_changed,
        session_keys,
    })
}

pub fn remove_all_profile_references(
    state: &mut QuickRunState,
    config: &mut CodezConfig,
    sessions: &mut CutexSessionStore,
    removed_name: &str,
) -> anyhow::Result<ProfileReferenceChanges> {
    let quick_state_changed = remove_profile_references(state, removed_name);
    let global_config_changed = remove_global_profile_references(config, removed_name);
    let session_keys = update_session_profile_references(sessions, removed_name, None)?;
    Ok(ProfileReferenceChanges {
        quick_state_changed,
        global_config_changed,
        session_keys,
    })
}

fn update_session_profile_references(
    sessions: &mut CutexSessionStore,
    old_name: &str,
    new_name: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    if new_name == Some(old_name) {
        return Ok(Vec::new());
    }
    let timestamp = chrono::Utc::now().to_rfc3339();
    let mut keys = Vec::new();
    for (key, record) in &mut sessions.sessions {
        if record.profile.as_deref() == Some(old_name) {
            record.profile = new_name.map(str::to_string);
            record.bump_durable_revision()?;
            record.updated_at = timestamp.clone();
            keys.push(key.clone());
        }
    }
    keys.sort();
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::session::model::CutexSessionRecord;

    fn session(key: &str, profile: &str) -> CutexSessionRecord {
        CutexSessionRecord::new_at(
            key.to_string(),
            None,
            "devbox".to_string(),
            "/tmp".to_string(),
            Some(profile.to_string()),
            "2026-08-05T00:00:00Z".to_string(),
        )
        .expect("session fixture")
    }

    #[test]
    fn rename_updates_quick_global_and_all_matching_durable_sessions() {
        let mut state = QuickRunState {
            last_global_profile: Some("old".to_string()),
            per_directory: [
                ("/one".to_string(), "old".to_string()),
                ("/two".to_string(), "keep".to_string()),
            ]
            .into_iter()
            .collect(),
        };
        let mut config = CodezConfig {
            default_profile: Some("old".to_string()),
            ..CodezConfig::default()
        };
        let mut sessions = CutexSessionStore::default();
        sessions
            .sessions
            .insert("cutex.two".to_string(), session("cutex.two", "old"));
        sessions
            .sessions
            .insert("cutex.one".to_string(), session("cutex.one", "old"));
        sessions
            .sessions
            .insert("cutex.keep".to_string(), session("cutex.keep", "keep"));

        let changes =
            rename_all_profile_references(&mut state, &mut config, &mut sessions, "old", "new")
                .expect("rename references");

        assert_eq!(
            changes,
            ProfileReferenceChanges {
                quick_state_changed: true,
                global_config_changed: true,
                session_keys: vec!["cutex.one".to_string(), "cutex.two".to_string()],
            }
        );
        assert_eq!(state.last_global_profile.as_deref(), Some("new"));
        assert_eq!(
            state.per_directory.get("/one").map(String::as_str),
            Some("new")
        );
        assert_eq!(
            state.per_directory.get("/two").map(String::as_str),
            Some("keep")
        );
        assert_eq!(config.default_profile.as_deref(), Some("new"));
        for key in ["cutex.one", "cutex.two"] {
            let record = sessions.sessions.get(key).expect("renamed session");
            assert_eq!(record.profile.as_deref(), Some("new"));
            assert_eq!(record.durable_revision(), 2);
            assert_ne!(record.updated_at, "2026-08-05T00:00:00Z");
        }
        let untouched = sessions
            .sessions
            .get("cutex.keep")
            .expect("untouched session");
        assert_eq!(untouched.profile.as_deref(), Some("keep"));
        assert_eq!(untouched.updated_at, "2026-08-05T00:00:00Z");
    }

    #[test]
    fn remove_clears_every_matching_reference_and_reports_noop_afterward() {
        let mut state = QuickRunState {
            last_global_profile: Some("old".to_string()),
            per_directory: [("/one".to_string(), "old".to_string())]
                .into_iter()
                .collect(),
        };
        let mut config = CodezConfig {
            default_profile: Some("old".to_string()),
            ..CodezConfig::default()
        };
        let mut sessions = CutexSessionStore::default();
        sessions
            .sessions
            .insert("cutex.one".to_string(), session("cutex.one", "old"));

        let changes = remove_all_profile_references(&mut state, &mut config, &mut sessions, "old")
            .expect("remove references");

        assert!(changes.quick_state_changed);
        assert!(changes.global_config_changed);
        assert_eq!(changes.session_keys, ["cutex.one"]);
        assert_eq!(state.last_global_profile, None);
        assert!(state.per_directory.is_empty());
        assert_eq!(config.default_profile, None);
        assert_eq!(sessions.sessions["cutex.one"].profile, None);

        assert_eq!(
            remove_all_profile_references(&mut state, &mut config, &mut sessions, "old")
                .expect("repeat remove references"),
            ProfileReferenceChanges::default()
        );
    }

    #[test]
    fn global_references_cover_quick_chat_default_and_new_session_defaults() {
        let defaults = crate::profiles::model::NewSessionDefaults {
            model: Some("glm-5".into()),
            reasoning: None,
        };
        let mut config = CodezConfig {
            default_profile: Some("keep".into()),
            quick_chat_default_profile: Some("old".into()),
            new_session_defaults: [("old".to_string(), defaults.clone())].into_iter().collect(),
            ..CodezConfig::default()
        };
        assert!(rename_global_profile_references(&mut config, "old", "new"));
        assert_eq!(config.default_profile.as_deref(), Some("keep"));
        assert_eq!(config.quick_chat_default_profile.as_deref(), Some("new"));
        assert_eq!(config.new_session_defaults.get("new"), Some(&defaults));
        assert!(!config.new_session_defaults.contains_key("old"));
        assert!(!rename_global_profile_references(&mut config, "old", "new"));
        assert!(remove_global_profile_references(&mut config, "new"));
        assert_eq!(config.quick_chat_default_profile, None);
        assert!(config.new_session_defaults.is_empty());
        assert_eq!(config.default_profile.as_deref(), Some("keep"));
    }
}
