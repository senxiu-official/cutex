//! Public profile facts for credential-free fork previews. Only the ordinary
//! profile presenter produces these; preview readers stat files but never open auth.
use super::model::StoredAccount;
use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant, SystemTime},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stamp {
    root: PathBuf,
    id: String,
    account: u64,
    config: Option<(u64, Option<SystemTime>, Option<SystemTime>)>,
    auth: Option<(u64, Option<SystemTime>, Option<SystemTime>)>,
}
#[derive(Clone, Debug)]
pub struct Facts {
    pub family: String,
    pub allow_switch: bool,
}
static CACHE: OnceLock<Mutex<HashMap<(PathBuf, String), (Stamp, Instant, Facts)>>> =
    OnceLock::new();
fn file_stamp(path: &Path) -> Option<Option<(u64, Option<SystemTime>, Option<SystemTime>)>> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_file() && !m.file_type().is_symlink() => {
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if m.file_attributes() & 0x400 != 0 {
                    return None;
                }
            }
            Some(Some((m.len(), m.modified().ok(), m.created().ok())))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(None),
        _ => None,
    }
}
fn at(root: PathBuf, account: &StoredAccount) -> Option<Stamp> {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    // Appearance, quotas and last_used_at can change while another agent uses
    // the profile. Only compatibility inputs invalidate these derived facts.
    serde_json::to_vec(&serde_json::json!({
        "id":account.id,"name":account.name,"cliKind":account.cli_kind,
        "source":account.source,"allowSessionSwitch":account.options.allow_session_switch,
        "claudeAccount":account.claude_account
    }))
    .ok()?
    .hash(&mut hash);
    let dir = root.join("profiles").join(&account.id);
    Some(Stamp {
        root,
        id: account.id.clone(),
        account: hash.finish(),
        config: file_stamp(&dir.join("config.toml"))?,
        auth: file_stamp(&dir.join("auth.json"))?,
    })
}
pub fn capture(account: &StoredAccount) -> Option<Stamp> {
    at(crate::config::paths::config_dir().ok()?, account)
}
pub fn remember(
    account: &StoredAccount,
    before: Option<Stamp>,
    family: String,
    allow_switch: bool,
) {
    let Some(stamp) = before.filter(|s| Some(s.clone()) == capture(account)) else {
        return;
    };
    if let Ok(mut cache) = CACHE.get_or_init(Default::default).lock() {
        if cache.len() >= 4096 {
            cache.clear();
        }
        cache.insert(
            (stamp.root.clone(), stamp.id.clone()),
            (
                stamp,
                Instant::now(),
                Facts {
                    family,
                    allow_switch,
                },
            ),
        );
    }
}
pub fn get(account: &StoredAccount) -> Option<Facts> {
    lookup(&capture(account)?)
}
fn lookup(stamp: &Stamp) -> Option<Facts> {
    let cache = CACHE.get_or_init(Default::default).lock().ok()?;
    let (previous, when, facts) = cache.get(&(stamp.root.clone(), stamp.id.clone()))?;
    (previous == stamp && when.elapsed() < Duration::from_secs(30)).then(|| facts.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn facts_are_home_bound_and_invalidated_without_reading_auth_contents() {
        let root = std::env::temp_dir().join(format!("fork-facts-{}", uuid::Uuid::new_v4()));
        let account = crate::profiles::lifecycle::new_claude_account("metadata-only");
        let dir = root.join("profiles").join(&account.id);
        std::fs::create_dir_all(&dir).unwrap();
        // Invalid credential content is deliberately never parsed by this module.
        std::fs::write(dir.join("auth.json"), "not-json").unwrap();
        let stamp = at(root.clone(), &account).unwrap();
        CACHE.get_or_init(Default::default).lock().unwrap().insert(
            (stamp.root.clone(), stamp.id.clone()),
            (
                stamp.clone(),
                Instant::now(),
                Facts {
                    family: "anthropic".into(),
                    allow_switch: true,
                },
            ),
        );
        assert_eq!(lookup(&stamp).unwrap().family, "anthropic");
        let mut used = account.clone();
        used.last_used_at = Some(chrono::Utc::now());
        assert!(lookup(&at(root.clone(), &used).unwrap()).is_some());
        let elsewhere = at(root.join("other-home"), &account).unwrap();
        assert!(lookup(&elsewhere).is_none());
        std::fs::write(dir.join("auth.json"), "changed length; still not json").unwrap();
        assert!(lookup(&at(root.clone(), &account).unwrap()).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}
