//! State backup and binary-rollback helpers (RFC §4.2 backup, §4.5).
//!
//! The store list is the r233 kit's `STATE_FILES` / `STATE_DIRS` in code.
//! Services must be stopped. SQLite databases are copied with the backup API
//! (a consistent snapshot; never db + -wal + -shm byte copies); every other
//! file is copied and verified by sha256 against the stopped home;
//! `home-manifest.json` lists both. `runtime/artifacts` and native homes are
//! never copied. A rollback never restores this backup over newer data by
//! itself: only the selection files ([`SELECTION_ITEMS`]) come back, newly
//! created stores are moved aside (never deleted).

use std::path::{Path, PathBuf};

use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};

/// Single files under the cutex home.
pub const STATE_FILES: &[&str] = &[
    "config.json",
    "accounts.json",
    "im-sessions.json",
    "state.json",
    "hosts.json",
    "cutex-sessions.json",
    "cutex-sessions.sqlite3",
    "usage-prices.json",
    "usage-prices-v2.json",
    "profile-name-history.json",
    "instance-id",
    "runtime/agent-bus-registry.json",
    "runtime/credential-custody.json",
    "runtime/light/deployment.json",
    "runtime/native-version-registry-v1.json",
    "claude-permission-rules.json",
];

/// Directories under the cutex home.
pub const STATE_DIRS: &[&str] = &[
    "profiles",
    "services",
    "pan",
    "claude-accounts",
    // r265 write-only stored profile keys (current key, version and the
    // retired keys kept for redaction; `profiles::stored_keys::KEYS_DIR`).
    "profile-keys",
    // r266 Kimi accounts (homes with the OAuth credential) and their registry.
    "kimi-accounts",
    "runtime/kimi",
    "runtime/task-service",
    "runtime/agent-management",
    "runtime/management-v2",
    "runtime/director-address",
    "runtime/storage",
    "runtime/profile-link",
    "runtime/claude",
    "runtime/asks",
    "runtime/owner-notify",
    "runtime/owner-pins",
    "runtime/documents",
    "runtime/owner-rotation",
    "runtime/profile-quota",
    "runtime/alarms",
    "runtime/usage-pricing",
    "runtime/file-reverts",
    "runtime/peers",
    "runtime/approvals",
    "runtime/browser",
    "runtime/inbound-mail",
    "runtime/nci-verification",
    "runtime/package-upgrade-runs",
    "archive",
    "notifications",
    "summary",
    "management-clients",
    "notes",
    "runtime/users",
    "runtime/workflows",
    "runtime/runtime-intent",
    "runtime/quota-continue",
    "runtime/profile-avatars",
    "runtime/live-thread-settings",
    "claude-skills",
];

/// What selects runtimes for new agents; a binary rollback puts these back.
pub const SELECTION_ITEMS: &[&str] =
    &["runtime/light/deployment.json", "runtime/native-version-registry-v1.json", "runtime/claude/deployment.json"];

/// Under these, nothing is copied (large or regenerated).
const SKIP_DIRS: &[&str] = &[
    "runtime/artifacts", "runtime/browser/profiles",
    // Disposable login homes contain browser profiles and caches, not the
    // installed account credential. Also exclude leftovers from older logins.
    "kimi-accounts/.login",
];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileRecord {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    #[serde(default)]
    pub sqlite: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupManifest {
    pub schema: String,
    pub created_at: String,
    pub home: String,
    pub items: Vec<String>,
    pub files: Vec<FileRecord>,
}

fn is_sqlite(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "sqlite3" || ext == "sqlite" || ext == "db")
}

fn is_sqlite_sidecar(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    ["-wal", "-shm", "-journal"].iter().any(|suffix| name.ends_with(suffix))
}

fn rel(home: &Path, path: &Path) -> String {
    path.strip_prefix(home).unwrap_or(path).to_string_lossy().replace('\\', "/")
}

fn verify_manifest(dest: &Path, expected: &BackupManifest) -> anyhow::Result<BackupManifest> {
    let manifest_path = dest.join("home-manifest.json");
    let manifest: BackupManifest = serde_json::from_slice(&std::fs::read(&manifest_path)?)
        .with_context(|| format!("read back {}", manifest_path.display()))?;
    ensure!(&manifest == expected, "backup manifest read-back does not match the files just copied");
    for record in &manifest.files {
        ensure!(super::manifest::safe_relative(&record.path), "unsafe backup manifest path `{}`", record.path);
        let path = dest.join("home").join(&record.path);
        let metadata = std::fs::symlink_metadata(&path).with_context(|| format!("verify backup file {}", path.display()))?;
        ensure!(metadata.is_file(), "backup manifest entry {} is not a regular file", record.path);
        ensure!(metadata.len() == record.bytes, "backup manifest byte count differs for {}", record.path);
        ensure!(super::manifest::file_sha256_hex(&path)? == record.sha256, "backup manifest digest differs for {}", record.path);
    }
    Ok(manifest)
}

fn open_snapshot_db(path: &Path, flags: rusqlite::OpenFlags) -> rusqlite::Result<rusqlite::Connection> {
    // Admit extended Windows pathnames for existing source databases and
    // unusually deep backup roots, independently of the default VFS limit.
    #[cfg(windows)]
    { rusqlite::Connection::open_with_flags_and_vfs(path, flags, "win32-longpath") }
    #[cfg(not(windows))]
    { rusqlite::Connection::open_with_flags(path, flags) }
}

struct SnapshotScratch(PathBuf);

impl Drop for SnapshotScratch {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
}

fn sync_file(file: &std::fs::File, path: &Path) -> anyhow::Result<()> {
    #[cfg(test)]
    durability_hook::before("file", path)?;
    file.sync_all().with_context(|| format!("sync backup file {}", path.display()))
}

fn publish_backup_file(from: &Path, to: &Path) -> anyhow::Result<()> {
    // Existing parents canonicalize to extended-length Windows paths. This
    // also admits long private temp names to the raw Win32 replacement API.
    let parent = to.parent().context("backup destination parent")?;
    let target = std::fs::canonicalize(parent)?.join(to.file_name().context("backup destination name")?);
    let source = std::fs::canonicalize(from)?;
    crate::config::atomic::replace_file_atomic(&source, &target)
        .with_context(|| format!("publish backup file {}", to.display()))
}

struct CopyScratch(PathBuf);

impl Drop for CopyScratch {
    fn drop(&mut self) {
        // Only this exact owned temp file is removed, never the source/target.
        #[cfg(windows)]
        if let Ok(metadata) = std::fs::metadata(&self.0) {
            if metadata.permissions().readonly() {
                let mut permissions = metadata.permissions();
                permissions.set_readonly(false);
                let _ = std::fs::set_permissions(&self.0, permissions);
            }
        }
        let _ = std::fs::remove_file(&self.0);
    }
}

fn copy_file_synced(source: &Path, target: &Path) -> anyhow::Result<()> {
    let parent = std::fs::canonicalize(target.parent().context("backup destination parent")?)?;
    let scratch = CopyScratch(parent.join(format!(".copy-{}", uuid::Uuid::new_v4().simple())));
    let mut input = std::fs::File::open(source)?;
    #[cfg(windows)]
    let mut output = crate::platform::private_fs::create_new_private_file(&scratch.0)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    #[cfg(not(windows))]
    let mut output = {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
        options.open(&scratch.0)?
    };
    std::io::copy(&mut input, &mut output).with_context(|| format!("copy {}", source.display()))?;
    // Open writable before restoring source permissions: Windows flushing
    // needs a writable handle even when the backed-up file is read-only.
    output.set_permissions(input.metadata()?.permissions())?;
    sync_file(&output, target)?;
    drop(output);
    publish_backup_file(&scratch.0, target)
}

fn sync_directory(path: &Path) -> anyhow::Result<()> {
    #[cfg(test)]
    durability_hook::before("directory", path)?;
    #[cfg(unix)]
    let directory = std::fs::File::open(path)?;
    #[cfg(windows)]
    let directory = {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
        // A read-only directory handle cannot be flushed on Windows. Request
        // GENERIC_WRITE and directory semantics, as proved on the NTFS builder.
        // If a filesystem does not support this, propagate the error: never
        // publish a manifest after silently skipping a durability barrier.
        std::fs::OpenOptions::new().write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path).with_context(|| format!("open backup directory for sync {}", path.display()))?
    };
    #[cfg(any(unix, windows))]
    directory.sync_all().with_context(|| format!("sync backup directory {}", path.display()))?;
    #[cfg(not(any(unix, windows)))]
    anyhow::bail!("backup directory sync is not supported on this platform");
    Ok(())
}

fn sync_directory_tree(path: &Path) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            sync_directory_tree(&entry.path())?;
        }
    }
    sync_directory(path)
}

#[cfg(test)]
mod durability_hook {
    use super::*;
    type Hook = Box<dyn FnMut(&str, &Path) -> anyhow::Result<()>>;
    thread_local! { static HOOK: std::cell::RefCell<Option<Hook>> = std::cell::RefCell::new(None); }
    pub(super) fn observe(hook: impl FnMut(&str, &Path) -> anyhow::Result<()> + 'static) -> Guard {
        HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook))); Guard
    }
    pub(super) struct Guard;
    impl Drop for Guard { fn drop(&mut self) { HOOK.with(|slot| *slot.borrow_mut() = None); } }
    pub(super) fn before(kind: &str, path: &Path) -> anyhow::Result<()> {
        HOOK.with(|slot| match slot.borrow_mut().as_mut() {
            Some(hook) => hook(kind, path), None => Ok(()),
        })
    }
}

/// Take a consistent snapshot into a short name directly under the backup
/// root, then publish its closed bytes at the (possibly deep) final path.
/// SQLite never opens the final destination or creates its sidecars.
pub fn sqlite_snapshot(source: &Path, dest: &Path, scratch_root: &Path) -> anyhow::Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let scratch = scratch_root.join(format!(".snapshot-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir(&scratch)?;
    let scratch = SnapshotScratch(scratch);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&scratch.0, std::fs::Permissions::from_mode(0o700))?;
    }
    // Canonicalize the existing parents: Rust emits extended-length Windows
    // paths, including UNC paths, without hand-built string prefixes.
    let source = std::fs::canonicalize(source)?;
    let snapshot = std::fs::canonicalize(&scratch.0)?.join("s.db");
    let connection = open_snapshot_db(
        &source,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open {}", source.display()))?;
    connection.busy_timeout(std::time::Duration::from_secs(30))?;
    let mut output = open_snapshot_db(&snapshot, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE)?;
    {
        let backup = rusqlite::backup::Backup::new(&connection, &mut output)?;
        let mut last_progress = std::time::Instant::now();
        loop {
            use rusqlite::backup::StepResult;
            match backup.step(128).with_context(|| format!("snapshot {}", source.display()))? {
                StepResult::Done => break,
                StepResult::More => last_progress = std::time::Instant::now(),
                StepResult::Busy | StepResult::Locked => {
                    ensure!(last_progress.elapsed() < std::time::Duration::from_secs(30), "snapshot {} stayed locked for 30s", source.display());
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                _ => anyhow::bail!("unexpected SQLite backup status for {}", source.display()),
            }
        }
    }
    let check: String = output.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    ensure!(check == "ok", "snapshot of {} failed its integrity check: {check}", source.display());
    // Explicit close before filesystem publication, especially on Windows.
    output.close().map_err(|(_, error)| error)?;
    let flushed = std::fs::OpenOptions::new().write(true).open(&snapshot)?;
    sync_file(&flushed, dest)?;
    drop(flushed);
    publish_backup_file(&snapshot, dest)?;
    Ok(())
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>, home: &Path) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            if SKIP_DIRS.contains(&rel(home, &path).as_str()) {
                continue;
            }
            walk(&path, out, home)?;
        } else if kind.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

fn top_level_metadata(path: &Path) -> anyhow::Result<Option<std::fs::Metadata>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("inspect backup item {}", path.display())),
    }
}

/// Copy the state of `home` into `dest/home/` and write
/// `dest/home-manifest.json`. Services must be stopped.
pub fn backup(home: &Path, dest: &Path) -> anyhow::Result<BackupManifest> {
    ensure!(top_level_metadata(&dest.join("home-manifest.json"))?.is_none(), "backup already has a committed manifest: {}", dest.display());
    let mut parents_to_sync = Vec::new();
    for parent in dest.ancestors().skip(1).filter(|path| !path.as_os_str().is_empty()) {
        parents_to_sync.push(parent.to_path_buf());
        if top_level_metadata(parent)?.is_some() { break; }
    }
    crate::management::client_tokens::ensure_private_dir(dest)?;
    let target_home = dest.join("home");
    std::fs::create_dir_all(&target_home)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut items = Vec::new();
    let mut sources = Vec::new();
    for item in STATE_FILES.iter().chain(STATE_DIRS) {
        let path = home.join(item);
        let Some(meta) = top_level_metadata(&path)? else { continue };
        if meta.is_dir() {
            walk(&path, &mut sources, home)?;
        } else if meta.is_file() {
            sources.push(path);
        } else {
            continue;
        }
        items.push(item.to_string());
    }
    let mut files = Vec::new();
    for source in sources {
        if is_sqlite_sidecar(&source) {
            continue;
        }
        let relative = rel(home, &source);
        let target = target_home.join(&relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if is_sqlite(&source) {
            sqlite_snapshot(&source, &target, dest)?;
            files.push(FileRecord {
                sha256: super::manifest::file_sha256_hex(&target)?,
                bytes: std::fs::metadata(&target)?.len(),
                path: relative,
                sqlite: true,
            });
            continue;
        }
        copy_file_synced(&source, &target)?;
        let original = super::manifest::file_sha256_hex(&source)?;
        let copied = super::manifest::file_sha256_hex(&target)?;
        ensure!(original == copied, "backup of {relative} does not match the stopped home");
        files.push(FileRecord { path: relative, sha256: copied, bytes: std::fs::metadata(&target)?.len(), sqlite: false });
    }
    let manifest = BackupManifest {
        schema: "cutex/upgrade-backup/v1".into(),
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        home: home.display().to_string(),
        items,
        files,
    };
    // Commit point: every file and each nested directory entry precedes the
    // durable manifest. Sync the root and its parent for a newly created run.
    sync_directory_tree(&target_home)?;
    sync_directory(dest)?;
    for parent in parents_to_sync { sync_directory(&parent)?; }
    #[cfg(test)]
    durability_hook::before("manifest", &dest.join("home-manifest.json"))?;
    crate::config::atomic::write_private_pretty_json_atomic(&dest.join("home-manifest.json"), &manifest, "backup manifest")?;
    // The general atomic helper syncs Unix parents. Explicitly require the
    // directory barrier here on Windows too, after the manifest name appears.
    sync_directory(dest)?;
    verify_manifest(dest, &manifest)
}

/// Move `home/<relative>` to `displaced/<relative>` (never deleted).
pub fn move_aside(home: &Path, relative: &str, displaced: &Path) -> anyhow::Result<bool> {
    ensure!(super::manifest::safe_relative(relative), "unsafe store path `{relative}`");
    let source = home.join(relative);
    if std::fs::symlink_metadata(&source).is_err() {
        return Ok(false);
    }
    let target = displaced.join(relative);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    ensure!(!target.exists(), "{} already exists", target.display());
    std::fs::rename(&source, &target).with_context(|| format!("move {} aside", source.display()))?;
    Ok(true)
}

/// Put the selection files of a backup back (current ones moved aside);
/// an item the backup lacks (none was selected then) is only moved aside.
pub fn restore_selection(backup: &Path, home: &Path, displaced: &Path) -> anyhow::Result<Vec<String>> {
    let _runtime_reference = crate::runtime::reference_guard::acquire_shared()?;
    let mut restored = Vec::new();
    for item in SELECTION_ITEMS {
        let saved = backup.join("home").join(item);
        let current = home.join(item);
        let saved_exists = match top_level_metadata(&saved)? {
            Some(metadata) => {
                ensure!(metadata.is_file(), "saved selection {} is not a regular file", saved.display());
                true
            }
            None => false,
        };
        let same = saved_exists
            && current.is_file()
            && super::manifest::file_sha256_hex(&saved)? == super::manifest::file_sha256_hex(&current)?;
        if same || (!saved_exists && std::fs::symlink_metadata(&current).is_err()) {
            continue;
        }
        if saved_exists {
            // Read the saved bytes before touching the live selection. Preserve
            // the outgoing selection durably without removing its current name:
            // a crash must leave either the old or restored complete document.
            let restored_bytes = std::fs::read(&saved)?;
            match std::fs::symlink_metadata(&current) {
                Ok(metadata) => {
                    ensure!(metadata.is_file(), "selection {} is not a regular file", current.display());
                    let outgoing = std::fs::read(&current)?;
                    let preserved = displaced.join(item);
                    match std::fs::read(&preserved) {
                        Ok(bytes) if bytes == outgoing => {}
                        Ok(_) => {
                            let newer = preserved.with_file_name(format!("{}.retry-{}", preserved.file_name().context("selection backup name")?.to_string_lossy(), uuid::Uuid::new_v4().simple()));
                            crate::config::atomic::write_private_bytes_atomic(&newer, &outgoing)?;
                            eprintln!("cutex rollback: selection changed since the first attempt; newer copy preserved at {}", newer.display());
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            crate::config::atomic::write_private_bytes_atomic(&preserved, &outgoing)?;
                        }
                        Err(error) => return Err(error).context("read displaced selection"),
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("inspect current selection"),
            }
            crate::config::atomic::write_private_bytes_atomic(&current, &restored_bytes)?;
        } else {
            move_aside(home, item, displaced)?;
        }
        restored.push(item.to_string());
    }
    Ok(restored)
}

/// Keep the backups of the newest `keep` runs (by run directory mtime).
pub fn prune(runs_dir: &Path, keep: usize) -> anyhow::Result<Vec<PathBuf>> {
    let mut backups: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(runs_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path().join("backup"))
        .filter(|path| path.is_dir())
        .filter_map(|path| Some((std::fs::metadata(&path).ok()?.modified().ok()?, path)))
        .collect();
    backups.sort_by(|a, b| b.0.cmp(&a.0));
    let mut removed = Vec::new();
    for (_, path) in backups.into_iter().skip(keep) {
        std::fs::remove_dir_all(&path)?;
        removed.push(path);
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_or_directory_sync_failure_never_publishes_a_manifest() {
        for failure in ["file", "directory"] {
            let home = temp("sync-fault-home");
            let dest = temp("sync-fault-dest");
            let source = home.join("config.json");
            std::fs::write(&source, b"original settings").unwrap();
            let hook = durability_hook::observe(move |kind, _| {
                if kind == failure { anyhow::bail!("injected {failure} sync failure"); }
                Ok(())
            });
            let result = backup(&home, &dest);
            drop(hook);
            assert!(result.is_err(), "{failure} sync failure must abort backup");
            assert!(!dest.join("home-manifest.json").exists());
            assert_eq!(std::fs::read(&source).unwrap(), b"original settings");
            // A failed pre-commit backup remains retryable.
            backup(&home, &dest).unwrap();
            for dir in [home, dest] { std::fs::remove_dir_all(dir).unwrap(); }
        }
    }

    #[test]
    fn backup_syncs_readonly_copies_and_nested_directories_before_manifest() {
        use std::{cell::RefCell, rc::Rc};
        let home = temp("sync-order-home");
        let dest = temp("sync-order-dest");
        let source = home.join("profiles/deep/auth.json");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, b"readonly settings").unwrap();
        let original_permissions = std::fs::metadata(&source).unwrap().permissions();
        let mut readonly = original_permissions.clone();
        readonly.set_readonly(true);
        std::fs::set_permissions(&source, readonly).unwrap();
        let seen = Rc::new(RefCell::new(Vec::new()));
        let events = seen.clone();
        let hook = durability_hook::observe(move |kind, path| {
            events.borrow_mut().push((kind.to_owned(), path.to_path_buf())); Ok(())
        });
        let result = backup(&home, &dest);
        drop(hook);
        std::fs::set_permissions(&source, original_permissions.clone()).unwrap();
        result.unwrap();
        let target = dest.join("home/profiles/deep/auth.json");
        assert_eq!(std::fs::read(&target).unwrap(), b"readonly settings");
        let events = seen.borrow();
        let manifest = events.iter().position(|(kind, _)| kind == "manifest").unwrap();
        assert!(events[..manifest].contains(&("file".to_owned(), target.clone())));
        let deep = dest.join("home/profiles/deep");
        let parent = dest.join("home/profiles");
        let deep_at = events.iter().position(|event| event == &("directory".to_owned(), deep.clone())).unwrap();
        let parent_at = events.iter().position(|event| event == &("directory".to_owned(), parent.clone())).unwrap();
        assert!(deep_at < parent_at && parent_at < manifest);
        assert!(std::fs::metadata(&target).unwrap().permissions().readonly());
        std::fs::set_permissions(&target, original_permissions).unwrap();
        for dir in [home, dest] { std::fs::remove_dir_all(dir).unwrap(); }
    }

    #[test]
    fn completed_backup_is_immutable_on_a_repeated_call() {
        let home = temp("immutable-home");
        let dest = temp("immutable-dest");
        std::fs::write(home.join("config.json"), b"before").unwrap();
        backup(&home, &dest).unwrap();
        let manifest = std::fs::read(dest.join("home-manifest.json")).unwrap();
        std::fs::write(home.join("config.json"), b"after").unwrap();
        assert!(backup(&home, &dest).is_err());
        assert_eq!(std::fs::read(dest.join("home/config.json")).unwrap(), b"before");
        assert_eq!(std::fs::read(dest.join("home-manifest.json")).unwrap(), manifest);
        for dir in [home, dest] { std::fs::remove_dir_all(dir).unwrap(); }
    }

    #[test]
    fn selection_retry_retains_both_outgoing_versions() {
        let root = temp("retry-changed-selection");
        let home = root.join("home");
        let backup = root.join("backup");
        let displaced = root.join("displaced");
        let item = SELECTION_ITEMS[0];
        for path in [home.join(item), backup.join("home").join(item), displaced.join(item)] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        }
        std::fs::write(home.join(item), b"newer current").unwrap();
        std::fs::write(backup.join("home").join(item), b"selected old").unwrap();
        std::fs::write(displaced.join(item), b"first current").unwrap();
        restore_selection(&backup, &home, &displaced).unwrap();
        assert_eq!(std::fs::read(home.join(item)).unwrap(), b"selected old");
        assert_eq!(std::fs::read(displaced.join(item)).unwrap(), b"first current");
        let retry = std::fs::read_dir(displaced.join(item).parent().unwrap()).unwrap()
            .map(|entry| entry.unwrap().path()).find(|path| path.file_name().unwrap().to_string_lossy().contains(".retry-")).unwrap();
        assert_eq!(std::fs::read(retry).unwrap(), b"newer current");
        std::fs::remove_dir_all(root).unwrap();
    }

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cutex-backup-{tag}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn home() -> (PathBuf, rusqlite::Connection) {
        let home = temp("home");
        std::fs::write(home.join("config.json"), b"{\"a\":1}").unwrap();
        std::fs::create_dir_all(home.join("runtime/light")).unwrap();
        std::fs::write(home.join("runtime/light/deployment.json"), b"old-selection").unwrap();
        std::fs::create_dir_all(home.join("runtime/asks/sub")).unwrap();
        std::fs::write(home.join("runtime/asks/sub/a.json"), b"ask").unwrap();
        for (relative, contents) in [
            ("runtime/director-address/ownership-v1.json", &b"director"[..]),
            ("runtime/storage/cleanup-v1.json", &b"cleanup"[..]),
            ("runtime/profile-link/metadata-v1.json", &b"profile-link"[..]),
        ] {
            let path = home.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        std::fs::create_dir_all(home.join("runtime/artifacts")).unwrap();
        std::fs::write(home.join("runtime/artifacts/big.bin"), b"never").unwrap();
        let db = rusqlite::Connection::open(home.join("cutex-sessions.sqlite3")).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE s(x); INSERT INTO s VALUES (1),(2);").unwrap();
        // The WAL is still open here: the snapshot must include its rows.
        (home, db)
    }

    #[test]
    fn backs_up_files_and_sqlite_snapshots() {
        let (home, source_db) = home();
        let dest = temp("dest");
        let manifest = backup(&home, &dest).unwrap();
        assert!(manifest.items.contains(&"config.json".to_string()));
        assert!(manifest.items.contains(&"runtime/asks".to_string()));
        for item in ["runtime/director-address", "runtime/storage", "runtime/profile-link"] {
            assert!(manifest.items.contains(&item.to_string()), "{item}");
            assert!(manifest.files.iter().any(|record| record.path.starts_with(&format!("{item}/"))), "{item}");
        }
        assert!(!manifest.items.iter().any(|i| i.contains("artifacts")));
        let db = manifest.files.iter().find(|f| f.path == "cutex-sessions.sqlite3").unwrap();
        assert!(db.sqlite);
        let copy = rusqlite::Connection::open(dest.join("home/cutex-sessions.sqlite3")).unwrap();
        let rows: i64 = copy.query_row("SELECT count(*) FROM s", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 2);
        assert!(!manifest.files.iter().any(|f| f.path.ends_with("-wal") || f.path.ends_with("-shm")));
        assert_eq!(std::fs::read(dest.join("home/runtime/asks/sub/a.json")).unwrap(), b"ask");
        let written: BackupManifest = serde_json::from_slice(&std::fs::read(dest.join("home-manifest.json")).unwrap()).unwrap();
        assert_eq!(written, manifest);
        drop(copy);
        drop(source_db);
        std::fs::remove_dir_all(dest).unwrap();
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn input_dispositions_backup_keeps_wal_and_existing_schema_and_rollback_keeps_new_decisions() {
        let home=temp("disposition-home");let dest=temp("disposition-backup");let displaced=temp("disposition-displaced");
        let relative="runtime/management-v2/input-dispositions.sqlite3";
        let ledger=home.join("runtime/management-v2/input-ledger.sqlite3");
        let first=crate::management::v2::input_disposal::request_at(&ledger,"recipient","old-input",chrono::Utc::now()).unwrap();
        // Keep a connection open so subsequent commits remain in the WAL.
        let source=rusqlite::Connection::open(home.join(relative)).unwrap();
        source.execute_batch("BEGIN;").unwrap();
        assert_eq!(source.query_row("SELECT COUNT(*) FROM owner_dispositions",[],|row|row.get::<_,i64>(0)).unwrap(),1);
        let second=crate::management::v2::input_disposal::request_at(&ledger,"recipient","wal-input",chrono::Utc::now()).unwrap();
        assert!(home.join(format!("{relative}-wal")).metadata().unwrap().len()>0);
        assert_eq!(source.pragma_query_value(None,"user_version",|row|row.get::<_,i64>(0)).unwrap(),1);
        let manifest=backup(&home,&dest).unwrap();
        assert!(manifest.files.iter().any(|file|file.path==relative&&file.sqlite));
        assert!(!manifest.files.iter().any(|file|file.path.ends_with("-wal")||file.path.ends_with("-shm")));
        verify_manifest(&dest,&manifest).unwrap();
        let copied=dest.join("home/runtime/management-v2/input-ledger.sqlite3");
        assert_eq!(crate::management::v2::input_disposal::request_at(&copied,"recipient","old-input",chrono::Utc::now()).unwrap().disposition_id,first.disposition_id);
        assert_eq!(crate::management::v2::input_disposal::request_at(&copied,"recipient","wal-input",chrono::Utc::now()).unwrap().disposition_id,second.disposition_id);
        let newest=crate::management::v2::input_disposal::request_at(&ledger,"recipient","after-backup",chrono::Utc::now()).unwrap();
        restore_selection(&dest,&home,&displaced).unwrap();
        assert_eq!(crate::management::v2::input_disposal::request_at(&ledger,"recipient","after-backup",chrono::Utc::now()).unwrap().disposition_id,newest.disposition_id,"binary rollback never overwrites newer disposition authority");
        drop(source);
        for path in [home,dest,displaced] {if path.exists(){std::fs::remove_dir_all(path).unwrap();}}
    }

    #[test]
    fn new_state_snapshots_and_worker_rollback_refusal_preserve_all_data() {
        let home = temp("r282-state");
        let dest = temp("r282-backup");
        let roots = ["runtime/owner-pins", "pan"];
        let mut databases = Vec::new();
        for (root, name) in roots.into_iter().zip(["owner-pins.sqlite3", "pan.sqlite"]) {
            let path = home.join(root).join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let db = rusqlite::Connection::open(&path).unwrap();
            db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE data(value TEXT); INSERT INTO data VALUES ('live WAL');").unwrap();
            databases.push(db);
        }
        for name in ["key", "credentials"] {
            std::fs::write(home.join("pan").join(name), b"private fixture").unwrap();
        }
        let store = crate::services::store::ServiceStore::new(home.join("services"), "fixture")
            .with_port_probe(|_| panic!("worker cannot allocate a port"));
        let request = serde_json::from_value(serde_json::json!({
            "slug": "background", "runtime": "worker", "command": ["unused-fixture"], "lifetime": "persistent"
        })).unwrap();
        store.register(request, &crate::services::store::Caller::Owner).unwrap();
        let registry = std::fs::read(store.registry_path()).unwrap();
        let custody = b"{\"version\":1,\"files\":{\"management\":[\"/private/token-file\"]}}";
        std::fs::write(home.join("runtime/credential-custody.json"), custody).unwrap();
        let manifest = backup(&home, &dest).unwrap();
        assert_eq!(std::fs::read(dest.join("home/runtime/credential-custody.json")).unwrap(), custody);
        assert!(manifest.files.iter().any(|file| file.path == "runtime/credential-custody.json"));
        for (root, name) in roots.into_iter().zip(["owner-pins.sqlite3", "pan.sqlite"]) {
            let relative = format!("{root}/{name}");
            assert!(manifest.items.iter().any(|item| item == root), "{root} omitted");
            assert!(manifest.files.iter().any(|file| file.path == relative && file.sqlite), "{relative} omitted");
            let copy = rusqlite::Connection::open(dest.join("home").join(relative)).unwrap();
            let value: String = copy.query_row("SELECT value FROM data", [], |row| row.get(0)).unwrap();
            assert_eq!(value, "live WAL");
        }
        for name in ["key", "credentials"] {
            assert_eq!(std::fs::read(dest.join("home/pan").join(name)).unwrap(), b"private fixture");
        }
        assert_eq!(std::fs::read(dest.join("home/services/registry.json")).unwrap(), registry);
        for result in [
            super::super::older_stores::preflight(&home, "r281").map(|_| Vec::new()),
            super::super::older_stores::prepare(&home, &dest.join("displaced"), "r281"),
            super::super::older_stores::export_hot_stores(&home, "r281", true),
        ] {
            assert!(result.unwrap_err().to_string().contains("worker services"));
            assert_eq!(std::fs::read(store.registry_path()).unwrap(), registry);
        }
        assert!(!dest.join("displaced").exists());
        drop(databases);
        for dir in [home, dest] { std::fs::remove_dir_all(dir).unwrap(); }
    }

    #[test]
    fn login_scratch_is_excluded_but_installed_credentials_are_backed_up() {
        let home = temp("login-home");
        let dest = temp("login-backup");
        let scratch = home.join("kimi-accounts/.login/kl-old/tmp/chrome/Default");
        std::fs::create_dir_all(&scratch).unwrap();
        // Deliberately invalid: backup must not even try to open a cache DB.
        std::fs::write(scratch.join("cache.db"), b"disposable browser cache").unwrap();
        let account = home.join("kimi-accounts/ka-real/credentials");
        std::fs::create_dir_all(&account).unwrap();
        std::fs::write(account.join("kimi-code.json"), b"installed credential fixture").unwrap();
        let manifest = backup(&home, &dest).unwrap();
        assert_eq!(manifest.files.len(), 1);
        assert_eq!(manifest.files[0].path, "kimi-accounts/ka-real/credentials/kimi-code.json");
        assert_eq!(std::fs::read(dest.join("home").join(&manifest.files[0].path)).unwrap(), b"installed credential fixture");
        assert!(!dest.join("home/kimi-accounts/.login").exists());
        assert!(scratch.join("cache.db").exists(), "backup never deletes source scratch");
        for dir in [home, dest] { std::fs::remove_dir_all(dir).unwrap(); }
    }

    #[cfg(windows)]
    #[test]
    fn windows_snapshot_handles_max_path_sidecars_and_deep_source_and_destination() {
        use std::os::windows::ffi::OsStrExt;
        for length in [254, 340] {
            let home = temp("long-source");
            let dest = temp("long-backup");
            let mut relative = PathBuf::from("profiles");
            while dest.join("home").join(&relative).join("s.db").as_os_str().encode_wide().count() + 42 < length {
                relative.push("a".repeat(40));
            }
            let remaining = length - dest.join("home").join(&relative).join("s.db").as_os_str().encode_wide().count() - 1;
            relative.push("b".repeat(remaining));
            relative.push("s.db");
            let source = home.join(&relative);
            let target = dest.join("home").join(&relative);
            assert_eq!(target.as_os_str().encode_wide().count(), length);
            std::fs::create_dir_all(source.parent().unwrap()).unwrap();
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            let source_path = std::fs::canonicalize(source.parent().unwrap()).unwrap().join("s.db");
            let db = open_snapshot_db(&source_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE).unwrap();
            db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE data(value TEXT); INSERT INTO data VALUES ('from live WAL');").unwrap();
            // Reproduce the old implementation with the default Windows VFS:
            // the pathname/sidecar allowance fails even though the DB is valid.
            let old = rusqlite::Connection::open_in_memory().unwrap();
            let error = old.execute("VACUUM INTO ?1", [target.to_str().unwrap()]).unwrap_err();
            assert_eq!(error.sqlite_error_code(), Some(rusqlite::ErrorCode::CannotOpen));
            if target.exists() { std::fs::remove_file(&target).unwrap(); }
            let manifest = backup(&home, &dest).unwrap();
            assert_eq!(manifest.files.len(), 1);
            assert!(manifest.files[0].sqlite);
            let target_path = std::fs::canonicalize(&target).unwrap();
            let copy = open_snapshot_db(&target_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
            let value: String = copy.query_row("SELECT value FROM data", [], |row| row.get(0)).unwrap();
            assert_eq!(value, "from live WAL");
            assert_eq!(copy.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0)).unwrap(), "ok");
            assert!(!std::fs::read_dir(&dest).unwrap().any(|entry| entry.unwrap().file_name().to_string_lossy().starts_with(".snapshot-")));
            drop(copy);
            drop(db);
            for dir in [home, dest] { std::fs::remove_dir_all(dir).unwrap(); }
        }
    }

    #[test]
    fn r258_state_roots_are_part_of_the_backup_contract() {
        for file in ["accounts.json", "cutex-sessions.json", "cutex-sessions.sqlite3"] {
            assert!(STATE_FILES.contains(&file), "{file}");
        }
        for dir in ["services", "runtime/users", "runtime/approvals", "runtime/agent-management", "runtime/profile-avatars",
            crate::profiles::stored_keys::KEYS_DIR, crate::profiles::kimi_accounts::HOMES_DIR, "runtime/kimi"] {
            assert!(STATE_DIRS.contains(&dir), "{dir}");
        }
    }

    #[test]
    fn manifest_verification_detects_a_changed_backup_file() {
        let (home, source_db) = home();
        let dest = temp("verify");
        let manifest = backup(&home, &dest).unwrap();
        std::fs::write(dest.join("home/runtime/storage/cleanup-v1.json"), b"changed").unwrap();
        let error = verify_manifest(&dest, &manifest).unwrap_err();
        assert!(error.to_string().contains("runtime/storage/cleanup-v1.json"), "{error:#}");
        drop(source_db);
        std::fs::remove_dir_all(dest).unwrap();
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn top_level_metadata_skips_only_not_found() {
        let root = temp("metadata");
        assert!(top_level_metadata(&root.join("missing")).unwrap().is_none());
        let error = top_level_metadata(Path::new("\0")).unwrap_err();
        assert_ne!(
            error.downcast_ref::<std::io::Error>().map(std::io::Error::kind),
            Some(std::io::ErrorKind::NotFound),
            "non-NotFound errors must fail the backup: {error:#}"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn selection_restore_and_move_aside_never_delete() {
        let (home, source_db) = home();
        let dest = temp("dest");
        backup(&home, &dest).unwrap();
        std::fs::write(home.join("runtime/light/deployment.json"), b"new-selection").unwrap();
        std::fs::create_dir_all(home.join("runtime/claude")).unwrap();
        std::fs::write(home.join("runtime/claude/deployment.json"), b"new-claude").unwrap();
        std::fs::create_dir_all(home.join("runtime/new-store")).unwrap();
        std::fs::write(home.join("runtime/new-store/x"), b"x").unwrap();
        let displaced = temp("displaced");
        let restored = restore_selection(&dest, &home, &displaced).unwrap();
        assert_eq!(restored, vec!["runtime/light/deployment.json".to_string(), "runtime/claude/deployment.json".to_string()]);
        assert_eq!(std::fs::read(home.join("runtime/light/deployment.json")).unwrap(), b"old-selection");
        assert!(!home.join("runtime/claude/deployment.json").exists(), "nothing was selected before");
        assert_eq!(std::fs::read(displaced.join("runtime/claude/deployment.json")).unwrap(), b"new-claude");
        assert_eq!(std::fs::read(displaced.join("runtime/light/deployment.json")).unwrap(), b"new-selection");
        assert!(move_aside(&home, "runtime/new-store", &displaced).unwrap());
        assert!(!move_aside(&home, "runtime/absent", &displaced).unwrap());
        assert!(move_aside(&home, "../x", &displaced).is_err());
        assert_eq!(std::fs::read(displaced.join("runtime/new-store/x")).unwrap(), b"x");
        drop(source_db);
        for dir in [home, dest, displaced] {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn interrupted_selection_restore_keeps_current_complete_and_can_retry() {
        use crate::config::atomic::test_hook::{self, Stage};
        let root = temp("selection-interrupted");
        let home = root.join("home");
        let backup = root.join("backup");
        let displaced = root.join("displaced");
        let item = SELECTION_ITEMS[0];
        let current = home.join(item);
        let saved = backup.join("home").join(item);
        for path in [&current, &saved] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        }
        let old = br#"{"selection":"current"}"#;
        let restored = br#"{"selection":"previous"}"#;
        std::fs::write(&current, old).unwrap();
        std::fs::write(&saved, restored).unwrap();
        let current_parent = current.parent().unwrap().to_path_buf();
        let hook = test_hook::observe(move |stage, path| {
            if stage == Stage::Written && path.parent() == Some(current_parent.as_path()) {
                panic!("injected interruption before selection replacement");
            }
        });
        let interrupted = std::panic::catch_unwind(|| restore_selection(&backup, &home, &displaced));
        drop(hook);
        assert!(interrupted.is_err(), "fault injection must reach the replacement boundary");
        assert_eq!(std::fs::read(&current).unwrap(), old);
        assert_eq!(std::fs::read(displaced.join(item)).unwrap(), old);
        restore_selection(&backup, &home, &displaced).unwrap();
        assert_eq!(std::fs::read(&current).unwrap(), restored);
        assert_eq!(std::fs::read(displaced.join(item)).unwrap(), old);
        assert!(restore_selection(&backup, &home, &displaced).unwrap().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_saved_selection_does_not_remove_current() {
        let root = temp("invalid-selection");
        let home = root.join("home");
        let backup = root.join("backup");
        let displaced = root.join("displaced");
        let item = SELECTION_ITEMS[0];
        let current = home.join(item);
        std::fs::create_dir_all(current.parent().unwrap()).unwrap();
        std::fs::write(&current, b"current selection").unwrap();
        // An unreadable/non-file backup must not be treated as an intentionally
        // absent old selection, which would move the valid current file away.
        std::fs::create_dir_all(backup.join("home").join(item)).unwrap();
        assert!(restore_selection(&backup, &home, &displaced).is_err());
        assert_eq!(std::fs::read(&current).unwrap(), b"current selection");
        assert!(!displaced.join(item).exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prune_keeps_the_newest_backups() {
        let runs = temp("runs");
        for i in 0..5 {
            std::fs::create_dir_all(runs.join(format!("run{i}/backup"))).unwrap();
            std::fs::write(runs.join(format!("run{i}/run.json")), b"{}").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let removed = prune(&runs, 3).unwrap();
        assert_eq!(removed.len(), 2);
        assert!(!runs.join("run0/backup").exists() && runs.join("run0/run.json").exists());
        assert!(runs.join("run4/backup").exists());
        std::fs::remove_dir_all(runs).unwrap();
    }
}
