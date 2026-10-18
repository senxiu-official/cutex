//! The permission part of a connected runtime's live native thread settings,
//! shared with other cutex processes (Q21).
//!
//! The runtime manager holds the live settings in memory (the resume
//! snapshot, then every `thread/settings/updated`; `AppServerManagedRuntimeStatus`),
//! which Management uses for `browser_open` (`browser_relay::owner::full_access`).
//! The Agent Bus is another process and decides managed-service approvals,
//! so the manager also writes the permission fields here, one small file per
//! session (`runtime/live-thread-settings/<cutex session id>.json`). A reader
//! uses a file only when it names the session record's current runtime
//! generation and thread; otherwise it falls back to the record's stored
//! launch permissions.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::session::model::CutexSessionRecord;

/// Permission fields kept from the native thread settings.
const PERMISSION_FIELDS: [&str; 4] = [
    "sandboxPolicy",
    "sandbox",
    "approvalPolicy",
    "activePermissionProfile",
];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveThreadSettings {
    pub cutex_session_id: String,
    pub thread_id: String,
    pub runtime_generation: u64,
    pub observed_at: String,
    /// The permission fields of the native thread settings only.
    pub settings: Value,
}

/// `runtime/live-thread-settings` under the cutex home.
pub fn default_dir() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::runtime_dir()?.join("live-thread-settings"))
}

fn file_in(dir: &Path, cutex_session_id: &str) -> Option<PathBuf> {
    let valid = !cutex_session_id.is_empty()
        && cutex_session_id.len() <= 200
        && cutex_session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        && !cutex_session_id.starts_with('.');
    valid.then(|| dir.join(format!("{cutex_session_id}.json")))
}

/// The permission fields of `settings` (`None` when it carries none).
fn permission_part(settings: &Value) -> Option<Value> {
    let kept: Map<String, Value> = PERMISSION_FIELDS
        .iter()
        .filter_map(|field| {
            settings
                .get(*field)
                .filter(|value| !value.is_null())
                .map(|value| (field.to_string(), value.clone()))
        })
        .collect();
    (!kept.is_empty()).then_some(Value::Object(kept))
}

/// Write the live settings of a session's runtime into `dir`. Settings
/// without permission fields leave an existing file alone.
pub fn record_at(
    dir: &Path,
    cutex_session_id: &str,
    thread_id: &str,
    runtime_generation: u64,
    settings: &Value,
) -> anyhow::Result<()> {
    let Some(settings) = permission_part(settings) else {
        return Ok(());
    };
    let path = file_in(dir, cutex_session_id)
        .ok_or_else(|| anyhow::anyhow!("invalid cutex session id"))?;
    if read(&path).is_some_and(|known| {
        known.thread_id == thread_id
            && known.runtime_generation == runtime_generation
            && known.settings == settings
    }) {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    crate::config::atomic::write_private_pretty_json_atomic(
        &path,
        &LiveThreadSettings {
            cutex_session_id: cutex_session_id.to_string(),
            thread_id: thread_id.to_string(),
            runtime_generation,
            observed_at: chrono::Utc::now().to_rfc3339(),
            settings,
        },
        "live thread settings",
    )
}

/// [`record_at`] in the default directory; best effort (a reader without the
/// file falls back to the record's launch permissions).
pub fn record(cutex_session_id: &str, thread_id: &str, runtime_generation: u64, settings: &Value) {
    if let Ok(dir) = default_dir() {
        let _ = record_at(&dir, cutex_session_id, thread_id, runtime_generation, settings);
    }
}

fn read(path: &Path) -> Option<LiveThreadSettings> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// The live permission settings of `record`'s current runtime from `dir`:
/// only a file for the record's current runtime generation and thread
/// counts (a stale one from an earlier runtime does not).
pub fn current_at(dir: &Path, record: &CutexSessionRecord) -> Option<Value> {
    let live = read(&file_in(dir, &record.cutex_session_id)?)?;
    (live.cutex_session_id == record.cutex_session_id
        && live.runtime_generation == record.runtime_generation
        && Some(live.thread_id.as_str()) == record.app_server_thread_id())
    .then_some(live.settings)
}

/// [`current_at`] in the default directory.
pub fn current_for(record: &CutexSessionRecord) -> Option<Value> {
    current_at(&default_dir().ok()?, record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record() -> CutexSessionRecord {
        let path = if cfg!(windows) { "C:\\work" } else { "/work" };
        let mut record = CutexSessionRecord::new(
            "cutex.00000000-0000-0000-0000-0000000000a1".into(),
            None,
            "host".into(),
            path.into(),
            None,
        )
        .unwrap();
        record.codex_session_id = Some("thread-1".into());
        record.runtime_generation = 3;
        record
    }

    #[test]
    fn live_settings_are_read_only_for_the_current_runtime() {
        let dir = std::env::temp_dir().join(format!("cutex-live-settings-{}", uuid::Uuid::new_v4().simple()));
        let mut record = record();
        assert_eq!(current_at(&dir, &record), None);
        let full = json!({"model":"m","approvalPolicy":"never","sandboxPolicy":{"type":"dangerFullAccess"}});
        record_at(&dir, &record.cutex_session_id, "thread-1", 3, &full).unwrap();
        // Only the permission fields are kept.
        assert_eq!(
            current_at(&dir, &record),
            Some(json!({"approvalPolicy":"never","sandboxPolicy":{"type":"dangerFullAccess"}}))
        );
        // Settings without permission fields keep the file.
        record_at(&dir, &record.cutex_session_id, "thread-1", 3, &json!({"model":"other"})).unwrap();
        assert!(current_at(&dir, &record).is_some());
        // Another runtime generation or thread: stale, not used.
        record.runtime_generation = 4;
        assert_eq!(current_at(&dir, &record), None);
        record.runtime_generation = 3;
        record.codex_session_id = Some("thread-2".into());
        assert_eq!(current_at(&dir, &record), None);
        // A session id that is not a plain file name is never written.
        assert!(record_at(&dir, "../x", "t", 1, &full).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
