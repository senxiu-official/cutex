//! Profile identity: everything internal relates to the stable profile id;
//! the name is display / input only (docs/profile-id-and-runtimes.md).
//!
//! - `current_name_for_id` / `id_for_name`: hot-path lookups for live runtime
//!   ingestion, served from a snapshot of `accounts.json` that is re-read only
//!   when the file's length or mtime changes.
//! - `available_profiles_message`: the error text for an unknown profile,
//!   listing the current names so an agent can recover (no aliases: an old
//!   name does not resolve after a rename).
//! - `record_rename` / `ProfileNameDirectory`: an append-only rename history
//!   plus the deleted-profile records, used only to attribute historical
//!   usage-ledger rows that carry a name but no id.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::profiles::model::AccountsStore;

pub const NAME_HISTORY_FILE: &str = "profile-name-history.json";
const MAX_HISTORY_ENTRIES: usize = 2000;

#[derive(Clone, Default)]
struct Snapshot {
    stamp: Option<(u64, Option<SystemTime>)>,
    by_id: HashMap<String, String>,
    by_name: HashMap<String, String>,
}

static SNAPSHOT: Mutex<Option<(PathBuf, Snapshot)>> = Mutex::new(None);

fn stamp(path: &Path) -> Option<(u64, Option<SystemTime>)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()))
}

fn with_snapshot<R>(read: impl FnOnce(&Snapshot) -> R) -> R {
    let Ok(path) = crate::config::paths::accounts_path() else {
        return read(&Snapshot::default());
    };
    let current = stamp(&path);
    let mut guard = SNAPSHOT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((cached_path, cached)) = guard.as_ref() {
        if *cached_path == path && cached.stamp == current && current.is_some() {
            return read(cached);
        }
    }
    let store = crate::profiles::settings_api::load_store().unwrap_or_default();
    let mut fresh = Snapshot { stamp: current, ..Snapshot::default() };
    for account in &store.accounts {
        fresh.by_id.insert(account.id.clone(), account.name.clone());
        fresh.by_name.insert(account.name.clone(), account.id.clone());
    }
    let result = read(&fresh);
    *guard = Some((path, fresh));
    result
}

/// Current name of the profile with this id.
pub fn current_name_for_id(id: &str) -> Option<String> {
    with_snapshot(|snapshot| snapshot.by_id.get(id.trim()).cloned())
}

/// Id of the profile currently called `name`.
pub fn id_for_name(name: &str) -> Option<String> {
    with_snapshot(|snapshot| snapshot.by_name.get(name.trim()).cloned())
}

/// The profile a runtime was launched on, resolved to its current name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LaunchedProfile {
    pub id: Option<String>,
    pub name: Option<String>,
}

impl LaunchedProfile {
    /// Current `(id, name)` for a launch recorded as `(id, name)`: the id wins
    /// (a rename shows the new name); a record without id is looked up by
    /// name; an id that no longer exists keeps the recorded name.
    pub fn resolve(id: Option<&str>, name: Option<&str>) -> Self {
        let id = id.map(str::trim).filter(|value| !value.is_empty());
        let name = name.map(str::trim).filter(|value| !value.is_empty());
        if let Some(id) = id {
            return Self {
                id: Some(id.to_string()),
                name: current_name_for_id(id).or_else(|| name.map(str::to_string)),
            };
        }
        Self { id: name.and_then(id_for_name), name: name.map(str::to_string) }
    }

    /// As [`Self::resolve`], for a runtime occurrence launched at
    /// `launched_at` (RFC 3339). A record without id (launched by r217) is
    /// traced through the rename history first: the profile that held
    /// `name` at launch time, under its current name. Runtime bindings are
    /// never rewritten on rename (receipts and the reviewed registration
    /// compare them byte for byte), so this is how an occurrence launched
    /// before a rename finds its profile.
    pub fn resolve_at(id: Option<&str>, name: Option<&str>, launched_at: Option<&str>) -> Self {
        let id = id.map(str::trim).filter(|value| !value.is_empty());
        let name = name.map(str::trim).filter(|value| !value.is_empty());
        if id.is_some() || name.is_none() {
            return Self::resolve(id, name);
        }
        let at = launched_at
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc));
        match ProfileNameDirectory::load().resolve(None, name, at) {
            Some(ResolvedProfile { id, name, state: ProfileState::Current }) => Self { id, name: Some(name) },
            _ => Self { id: None, name: name.map(str::to_string) },
        }
    }

    /// Id when known, else the name (inputs accepting either).
    pub fn key(&self) -> Option<&str> {
        self.id.as_deref().or(self.name.as_deref())
    }
}

/// Current profile names, sorted.
pub fn available_profile_names(store: &AccountsStore) -> Vec<String> {
    let mut names: Vec<String> = store.accounts.iter().map(|account| account.name.clone()).collect();
    names.sort();
    names
}

/// Current profile names from the store on disk (empty when unreadable).
pub fn current_profile_names() -> Vec<String> {
    crate::profiles::settings_api::load_store()
        .map(|store| available_profile_names(&store))
        .unwrap_or_default()
}

/// `Profile not found: X. Available profiles: a, b` (names only; an id is
/// accepted too).
pub fn unknown_profile_message(requested: &str, available: &[String]) -> String {
    let list = if available.is_empty() { "(none)".to_string() } else { available.join(", ") };
    format!(
        "Profile not found (by name or id): {}. Available profiles: {list}",
        requested.trim()
    )
}

// ---------------------------------------------------------------------------
// Rename history (usage attribution only; never resolves inputs)

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NameHistory {
    #[serde(default = "history_version")]
    pub version: u32,
    #[serde(default)]
    pub renames: Vec<RenameEntry>,
}

fn history_version() -> u32 {
    1
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RenameEntry {
    pub profile_id: String,
    pub from: String,
    pub to: String,
    pub renamed_at: String,
}

pub fn history_path() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::config_dir()?.join(NAME_HISTORY_FILE))
}

pub fn load_history_at(path: &Path) -> NameHistory {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<NameHistory>(&bytes).ok())
        .unwrap_or(NameHistory { version: 1, renames: Vec::new() })
}

/// Append one rename (best effort for the caller; bounded).
pub fn record_rename(profile_id: &str, from: &str, to: &str) -> anyhow::Result<()> {
    let path = history_path()?;
    let mut history = load_history_at(&path);
    history.version = 1;
    history.renames.push(RenameEntry {
        profile_id: profile_id.to_string(),
        from: from.to_string(),
        to: to.to_string(),
        renamed_at: Utc::now().to_rfc3339(),
    });
    if history.renames.len() > MAX_HISTORY_ENTRIES {
        let excess = history.renames.len() - MAX_HISTORY_ENTRIES;
        history.renames.drain(..excess);
    }
    crate::config::atomic::write_private_pretty_json_atomic(&path, &history, "profile name history")
}

// ---------------------------------------------------------------------------
// Usage attribution

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ProfileState {
    /// Maps to a profile that exists now (shown under its current name).
    Current,
    /// Deleted, or a name no current profile can be traced to.
    Deleted,
}

impl ProfileState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Deleted => "deleted",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedProfile {
    pub id: Option<String>,
    pub name: String,
    pub state: ProfileState,
}

/// Current profiles, rename history and deleted-profile records, loaded once
/// per usage query.
#[derive(Clone, Debug, Default)]
pub struct ProfileNameDirectory {
    current_by_id: HashMap<String, String>,
    current_by_name: HashMap<String, String>,
    /// name -> [(profile id, renamed away at)]
    renamed_from: HashMap<String, Vec<(String, DateTime<Utc>)>>,
    /// name -> deleted profile id (newest deletion wins)
    deleted_by_name: HashMap<String, String>,
}

impl ProfileNameDirectory {
    pub fn load() -> Self {
        let store = crate::profiles::settings_api::load_store().unwrap_or_default();
        let history = history_path().map(|path| load_history_at(&path)).unwrap_or_default();
        let deleted = crate::config::paths::config_dir()
            .map(|dir| load_deleted_profiles(&dir.join(crate::profiles::lifecycle::DELETED_PROFILES_DIR)))
            .unwrap_or_default();
        Self::from_parts(&store, &history, &deleted)
    }

    pub fn from_parts(store: &AccountsStore, history: &NameHistory, deleted: &[(String, String)]) -> Self {
        let mut directory = Self::default();
        for account in &store.accounts {
            directory.current_by_id.insert(account.id.clone(), account.name.clone());
            directory.current_by_name.insert(account.name.clone(), account.id.clone());
        }
        for entry in &history.renames {
            let Ok(at) = DateTime::parse_from_rfc3339(&entry.renamed_at) else {
                continue;
            };
            directory
                .renamed_from
                .entry(entry.from.clone())
                .or_default()
                .push((entry.profile_id.clone(), at.with_timezone(&Utc)));
        }
        for list in directory.renamed_from.values_mut() {
            list.sort_by_key(|(_, at)| *at);
        }
        for (id, name) in deleted {
            directory.deleted_by_name.insert(name.clone(), id.clone());
        }
        directory
    }

    pub fn current_name(&self, id: &str) -> Option<&str> {
        self.current_by_id.get(id).map(String::as_str)
    }

    /// Attribute one ledger row recorded as `(id, name)` at `observed_at`.
    pub fn resolve(&self, id: Option<&str>, name: Option<&str>, observed_at: Option<DateTime<Utc>>) -> Option<ResolvedProfile> {
        let id = id.filter(|value| !value.is_empty());
        let name = name.filter(|value| !value.is_empty());
        if let Some(id) = id {
            if let Some(current) = self.current_by_id.get(id) {
                return Some(ResolvedProfile { id: Some(id.to_string()), name: current.clone(), state: ProfileState::Current });
            }
            return Some(ResolvedProfile {
                id: Some(id.to_string()),
                name: name.map(str::to_string).unwrap_or_else(|| id.to_string()),
                state: ProfileState::Deleted,
            });
        }
        let name = name?;
        // The profile that was called `name` at the row's time and renamed
        // away afterwards (earliest such rename).
        if let (Some(at), Some(renames)) = (observed_at, self.renamed_from.get(name)) {
            if let Some((profile_id, _)) = renames.iter().find(|(_, renamed_at)| *renamed_at > at) {
                // The id already folds in every later rename of that profile.
                return Some(match self.current_by_id.get(profile_id) {
                    Some(current) => ResolvedProfile { id: Some(profile_id.clone()), name: current.clone(), state: ProfileState::Current },
                    None => ResolvedProfile { id: Some(profile_id.clone()), name: name.to_string(), state: ProfileState::Deleted },
                });
            }
        }
        if let Some(current_id) = self.current_by_name.get(name) {
            return Some(ResolvedProfile { id: Some(current_id.clone()), name: name.to_string(), state: ProfileState::Current });
        }
        if let Some(renames) = self.renamed_from.get(name) {
            // No timestamp to compare: the latest profile that carried it.
            if let Some((profile_id, _)) = renames.last() {
                if let Some(current) = self.current_by_id.get(profile_id) {
                    return Some(ResolvedProfile { id: Some(profile_id.clone()), name: current.clone(), state: ProfileState::Current });
                }
            }
        }
        Some(ResolvedProfile {
            id: self.deleted_by_name.get(name).cloned(),
            name: name.to_string(),
            state: ProfileState::Deleted,
        })
    }
}

/// `(id, name)` of every readable `<deleted-profiles>/<ts>-<id>/profile.json`,
/// oldest first (read-only).
pub fn load_deleted_profiles(dir: &Path) -> Vec<(String, String)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut records: Vec<(String, String, String)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let folder = entry.file_name().to_string_lossy().into_owned();
            let bytes = std::fs::read(entry.path().join("profile.json")).ok()?;
            let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
            let id = value.get("id")?.as_str()?.to_string();
            let name = value.get("name")?.as_str()?.to_string();
            Some((folder, id, name))
        })
        .collect();
    records.sort();
    records.into_iter().map(|(_, id, name)| (id, name)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(accounts: &[(&str, &str)]) -> AccountsStore {
        serde_json::from_value(serde_json::json!({
            "version": crate::profiles::model::STORE_VERSION,
            "active_account_id": null,
            "accounts": accounts.iter().map(|(id, name)| serde_json::json!({
                "id": id, "name": name, "email": null, "plan_type": null, "last_used_at": null,
            })).collect::<Vec<_>>(),
        }))
        .unwrap()
    }

    fn at(value: &str) -> Option<DateTime<Utc>> {
        Some(DateTime::parse_from_rfc3339(value).unwrap().with_timezone(&Utc))
    }

    #[test]
    fn unknown_profile_message_lists_current_names() {
        let names = available_profile_names(&store(&[("b", "zeta"), ("a", "alpha")]));
        let message = unknown_profile_message(" old ", &names);
        assert_eq!(message, "Profile not found (by name or id): old. Available profiles: alpha, zeta");
        assert!(unknown_profile_message("x", &[]).ends_with("(none)"));
    }

    #[test]
    fn directory_maps_ids_history_names_and_deleted_records() {
        let history = NameHistory {
            version: 1,
            renames: vec![
                RenameEntry { profile_id: "id-1".into(), from: "old".into(), to: "mid".into(), renamed_at: "2026-10-05T00:00:00Z".into() },
                RenameEntry { profile_id: "id-1".into(), from: "mid".into(), to: "new".into(), renamed_at: "2026-10-07T00:00:00Z".into() },
            ],
        };
        // A new profile took over the name "old" after the rename.
        let directory = ProfileNameDirectory::from_parts(
            &store(&[("id-1", "new"), ("id-2", "old")]),
            &history,
            &[("id-9".into(), "GLM".into())],
        );
        let current = |id: &str, name: &str| ResolvedProfile { id: Some(id.into()), name: name.into(), state: ProfileState::Current };
        // By id: current name.
        assert_eq!(directory.resolve(Some("id-1"), Some("old"), None), Some(current("id-1", "new")));
        // No id, before the rename: the renamed profile.
        assert_eq!(directory.resolve(None, Some("old"), at("2026-10-01T00:00:00Z")), Some(current("id-1", "new")));
        assert_eq!(directory.resolve(None, Some("mid"), at("2026-10-06T00:00:00Z")), Some(current("id-1", "new")));
        // No id, after the rename: the profile now called "old".
        assert_eq!(directory.resolve(None, Some("old"), at("2026-10-08T00:00:00Z")), Some(current("id-2", "old")));
        // Deleted record and unknown names keep the historical name.
        assert_eq!(
            directory.resolve(None, Some("GLM"), at("2026-10-08T00:00:00Z")),
            Some(ResolvedProfile { id: Some("id-9".into()), name: "GLM".into(), state: ProfileState::Deleted })
        );
        assert_eq!(
            directory.resolve(None, Some("hazel"), None),
            Some(ResolvedProfile { id: None, name: "hazel".into(), state: ProfileState::Deleted })
        );
        // An id that no longer exists.
        assert_eq!(
            directory.resolve(Some("id-gone"), Some("lxx"), None),
            Some(ResolvedProfile { id: Some("id-gone".into()), name: "lxx".into(), state: ProfileState::Deleted })
        );
        assert_eq!(directory.resolve(None, None, None), None);
    }

    #[test]
    fn deleted_profile_records_are_read_from_folders() {
        let dir = std::env::temp_dir().join(format!("cutex-deleted-profiles-{}", uuid::Uuid::new_v4()));
        for (folder, body) in [
            ("20260101T000000Z-id-a", serde_json::json!({"id": "id-a", "name": "GLM"})),
            ("20260201T000000Z-id-b", serde_json::json!({"id": "id-b", "name": "GLM"})),
        ] {
            std::fs::create_dir_all(dir.join(folder)).unwrap();
            std::fs::write(dir.join(folder).join("profile.json"), body.to_string()).unwrap();
        }
        std::fs::create_dir_all(dir.join("junk")).unwrap();
        let records = load_deleted_profiles(&dir);
        assert_eq!(records, vec![("id-a".to_string(), "GLM".to_string()), ("id-b".to_string(), "GLM".to_string())]);
        let directory = ProfileNameDirectory::from_parts(&AccountsStore::default(), &NameHistory::default(), &records);
        assert_eq!(directory.resolve(None, Some("GLM"), None).unwrap().id.as_deref(), Some("id-b"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
