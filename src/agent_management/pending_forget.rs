//! r271 (051 §2): sessions whose durable record was deleted while their Agent
//! Management roster row may still exist. Session delete holds the session
//! lane, so it may only try the host fence for the roster cleanup; when the
//! fence is busy the lane-free sweep finishes it. Until then this marker makes
//! every roster/authority reader treat the id as gone, whatever the row says:
//! no routing authority, no Operator grant, no project-pause membership, and
//! `current_name_snapshot` leaves the row out instead of failing.
//!
//! Written before the durable record is removed and cleared once the roster
//! row is gone. One small private file under the runtime dir, independent of
//! the host fence; readers use a stamp-checked cache (no parse per call).
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use crate::platform::file_stamp::FileStamp;

const FILE_NAME: &str = "deleted-sessions-pending-forget.json";

fn path() -> anyhow::Result<std::path::PathBuf> {
    Ok(crate::config::paths::runtime_dir()?
        .join("agent-management")
        .join(FILE_NAME))
}

struct Cached {
    stamp: Option<FileStamp>,
    ids: Arc<BTreeSet<String>>,
}

static CACHE: Mutex<Option<Cached>> = Mutex::new(None);
static WRITE: Mutex<()> = Mutex::new(());

fn read(path: &std::path::Path) -> anyhow::Result<BTreeSet<String>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeSet::new()),
        Err(error) => Err(error.into()),
    }
}

/// The current set (empty when the file is absent or unreadable: a reader
/// never fails because of this marker).
pub fn pending() -> Arc<BTreeSet<String>> {
    let Ok(path) = path() else {
        return Arc::default();
    };
    let stamp = FileStamp::of(&path);
    let mut cache = CACHE.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(cached) = cache.as_ref() {
        if cached.stamp == stamp {
            return Arc::clone(&cached.ids);
        }
    }
    let ids = Arc::new(read(&path).unwrap_or_default());
    *cache = Some(Cached {
        stamp,
        ids: Arc::clone(&ids),
    });
    ids
}

/// Whether `id` was deleted and its roster row may still be present.
pub fn contains(id: &str) -> bool {
    pending().contains(id)
}

fn update(change: impl FnOnce(&mut BTreeSet<String>) -> bool) -> anyhow::Result<()> {
    let _write = WRITE.lock().unwrap_or_else(|error| error.into_inner());
    let path = path()?;
    let mut ids = read(&path)?;
    if !change(&mut ids) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::config::atomic::write_private_pretty_json_atomic(
        &path,
        &ids,
        "deleted sessions pending roster cleanup",
    )?;
    *CACHE.lock().unwrap_or_else(|error| error.into_inner()) = None;
    Ok(())
}

/// Session delete, before the durable record is removed.
pub fn mark(id: &str) -> anyhow::Result<()> {
    update(|ids| ids.insert(id.to_string()))
}

/// The roster row is gone (or never existed).
pub fn clear(id: &str) -> anyhow::Result<()> {
    update(|ids| ids.remove(id))
}
