//! Launch-selected credential custody shared by independent daemon processes.
//! The manifest stores paths only; registration never persists token contents.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use super::FileIdentity;
use super::credential_locations::{key, same_file_family};
use crate::platform::file_stamp::FileStamp;

pub(super) const MANIFEST: &str = "runtime/credential-custody.json";
#[derive(Default, Serialize, Deserialize)]
struct Manifest { version: u32, files: BTreeMap<String, Vec<PathBuf>> }
#[cfg(test)]
#[test]
#[ignore = "requires an explicitly marked release business copy"]
fn release_data_copy_custody_validate_and_reopen() {
    let home = PathBuf::from(std::env::var_os("CUTEX_RELEASE_DATA_COPY").expect("explicit copy"));
    assert!(home.is_absolute() && home.join(".cutex-release-data-copy").is_file());
    assert_eq!(crate::config::paths::config_dir().unwrap().canonicalize().unwrap(), home.canonicalize().unwrap());
    for pass in 0..2 {
        let manifest = read(&home).unwrap();
        // Decode the custody manifest only. Never resolve or open its credential paths.
        eprintln!("RELEASE DATA COPY custody pass={pass} sources={}", manifest.files.len());
    }
}
fn read(home: &Path) -> anyhow::Result<Manifest> {
    match std::fs::read(home.join(MANIFEST)) {
        Ok(bytes) => {
            let value: Manifest = serde_json::from_slice(&bytes)?;
            anyhow::ensure!(value.version == 1, "unsupported launch credential custody version");
            Ok(value)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Manifest { version: 1, ..Default::default() }),
        Err(error) => Err(error.into()),
    }
}
pub(super) fn paths(home: &Path) -> Vec<PathBuf> {
    read(home).map(|value| value.files.into_values().flatten().collect()).unwrap_or_default()
}
pub(super) fn register(source: &str, path: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(!source.is_empty() && source.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c)), "invalid credential custody source");
    let home = crate::config::paths::config_dir()?;
    let runtime = home.join("runtime");
    crate::management::client_tokens::ensure_private_dir(&runtime)?;
    let lock_path = runtime.join("credential-custody.lock");
    #[cfg(unix)]
    let lock = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(&lock_path)?
    };
    #[cfg(windows)]
    let lock = match crate::platform::private_fs::create_new_private_file(&lock_path) {
        Ok(file) => file,
        Err(error) if error.io_kind() == std::io::ErrorKind::AlreadyExists => std::fs::OpenOptions::new().read(true).write(true).open(&lock_path)?,
        Err(error) => return Err(error.into()),
    };
    fs2::FileExt::lock_exclusive(&lock)?;
    let mut manifest = read(&home).context("read launch credential custody")?;
    let saved = manifest.files.entry(source.to_string()).or_default();
    let mut changed = false;
    let absolute = std::path::absolute(path)?;
    let mut candidates = vec![absolute.clone()];
    if crate::artifacts::paths::validate_resolved_local_path(&absolute).is_ok() {
        if let Ok(real) = std::fs::canonicalize(&absolute) { candidates.push(real); }
    }
    for path in candidates {
        if !saved.iter().any(|old| key(old) == key(&path)) { saved.push(path); changed = true; }
    }
    if changed { crate::config::atomic::write_private_pretty_json_atomic(&home.join(MANIFEST), &manifest, "launch credential custody")?; }
    drop(lock);
    if changed { super::refresh_secret_path_cache(); }
    Ok(())
}
struct Snapshot { names: Vec<Vec<String>>, identities: Vec<FileIdentity> }
impl Snapshot {
    fn matches(&self, path: &Path, identity: Option<FileIdentity>) -> bool {
        if self.names.is_empty() && self.identities.is_empty() { return false; }
        let key = key(path);
        self.names.iter().any(|root| same_file_family(root, &key))
            || identity.is_some_and(|id| self.identities.contains(&id))
    }
}
struct Cached {
    home: PathBuf,
    manifest: Option<FileStamp>,
    paths: Vec<PathBuf>,
    stamps: Vec<Option<FileStamp>>,
    snapshot: Arc<Snapshot>,
}
fn stamp(path: &Path) -> Option<FileStamp> {
    crate::artifacts::paths::validate_resolved_local_path(path).ok()?;
    FileStamp::of(path)
}
fn current() -> Arc<Snapshot> {
    static CACHE: OnceLock<RwLock<Option<Arc<Cached>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| RwLock::new(None));
    let home = crate::config::paths::config_dir().unwrap_or_default();
    let manifest = FileStamp::of(&home.join(MANIFEST));
    let previous = cache.read().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref().filter(|old| old.home == home).cloned();
    let paths = if let Some(old) = previous.as_ref().filter(|old| old.manifest == manifest) { old.paths.clone() }
        else { match read(&home) { Ok(value) => value.files.into_values().flatten().collect(), Err(_) => return previous.map(|old| old.snapshot.clone()).unwrap_or_else(|| Arc::new(Snapshot { names: vec![], identities: vec![] })) } };
    let stamps = paths.iter().map(|path| stamp(path)).collect::<Vec<_>>();
    if let Some(old) = previous.as_ref().filter(|old| old.manifest == manifest && old.stamps == stamps) { return old.snapshot.clone(); }
    // This small explicit-file cache never traverses a directory or opens SMB.
    // IO is outside the shared lock. A listing pins it once for every entry.
    let mut built = Snapshot { names: paths.iter().map(|path| key(path)).collect(), identities: Vec::new() };
    for path in &paths {
        if crate::artifacts::paths::validate_resolved_local_path(path).is_ok() {
            if let Some((real, identity)) = super::secret_path_cache::inspect_credential_file(path) {
                built.names.push(key(&real));
                built.identities.push(identity);
            }
        }
    }
    let snapshot = Arc::new(built);
    *cache.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(Cached { home, manifest, paths, stamps, snapshot: snapshot.clone() }));
    snapshot
}
thread_local! { static PINNED: std::cell::RefCell<Option<Arc<Snapshot>>> = const { std::cell::RefCell::new(None) }; }
pub(super) struct RequestGuard(Option<Arc<Snapshot>>);
impl Drop for RequestGuard { fn drop(&mut self) { PINNED.with(|slot| *slot.borrow_mut() = self.0.take()); } }
pub(super) fn pin() -> RequestGuard {
    let snapshot = if super::narrowed_credential_read() { Some(current()) } else { None };
    RequestGuard(PINNED.with(|slot| std::mem::replace(&mut *slot.borrow_mut(), snapshot)))
}
pub(super) fn matches(path: &Path, identity: Option<FileIdentity>) -> bool {
    PINNED.with(|slot| slot.borrow().clone()).unwrap_or_else(current).matches(path, identity)
}
