//! State backup and binary-rollback helpers (RFC §4.2 backup, §4.5).
//!
//! The store list is the r233 kit's `STATE_FILES` / `STATE_DIRS` in code.
//! Services must be stopped. SQLite databases are copied with `VACUUM INTO`
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
    "runtime/light/deployment.json",
    "runtime/native-version-registry-v1.json",
    "claude-permission-rules.json",
];

/// Directories under the cutex home.
pub const STATE_DIRS: &[&str] = &[
    "profiles",
    "services",
    "claude-accounts",
    "runtime/task-service",
    "runtime/agent-management",
    "runtime/management-v2",
    "runtime/claude",
    "runtime/asks",
    "runtime/owner-notify",
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
const SKIP_DIRS: &[&str] = &["runtime/artifacts", "runtime/browser/profiles"];

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

/// A consistent copy of a SQLite database (`VACUUM INTO`).
pub fn sqlite_snapshot(source: &Path, dest: &Path) -> anyhow::Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(dest);
    let connection = rusqlite::Connection::open_with_flags(
        source,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open {}", source.display()))?;
    connection.busy_timeout(std::time::Duration::from_secs(30))?;
    connection
        .execute("VACUUM INTO ?1", [dest.to_string_lossy().as_ref()])
        .with_context(|| format!("snapshot {}", source.display()))?;
    let check: String = rusqlite::Connection::open_with_flags(dest, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    ensure!(check == "ok", "snapshot of {} failed its integrity check: {check}", source.display());
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

/// Copy the state of `home` into `dest/home/` and write
/// `dest/home-manifest.json`. Services must be stopped.
pub fn backup(home: &Path, dest: &Path) -> anyhow::Result<BackupManifest> {
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
        let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
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
            sqlite_snapshot(&source, &target)?;
            files.push(FileRecord {
                sha256: super::manifest::file_sha256_hex(&target)?,
                bytes: std::fs::metadata(&target)?.len(),
                path: relative,
                sqlite: true,
            });
            continue;
        }
        std::fs::copy(&source, &target).with_context(|| format!("copy {}", source.display()))?;
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
    crate::config::atomic::write_private_pretty_json_atomic(&dest.join("home-manifest.json"), &manifest, "backup manifest")?;
    Ok(manifest)
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
    let mut restored = Vec::new();
    for item in SELECTION_ITEMS {
        let saved = backup.join("home").join(item);
        let current = home.join(item);
        let same = saved.is_file()
            && current.is_file()
            && super::manifest::file_sha256_hex(&saved)? == super::manifest::file_sha256_hex(&current)?;
        if same || (!saved.is_file() && std::fs::symlink_metadata(&current).is_err()) {
            continue;
        }
        move_aside(home, item, displaced)?;
        if saved.is_file() {
            if let Some(parent) = current.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&saved, &current)?;
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

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cutex-backup-{tag}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn home() -> PathBuf {
        let home = temp("home");
        std::fs::write(home.join("config.json"), b"{\"a\":1}").unwrap();
        std::fs::create_dir_all(home.join("runtime/light")).unwrap();
        std::fs::write(home.join("runtime/light/deployment.json"), b"old-selection").unwrap();
        std::fs::create_dir_all(home.join("runtime/asks/sub")).unwrap();
        std::fs::write(home.join("runtime/asks/sub/a.json"), b"ask").unwrap();
        std::fs::create_dir_all(home.join("runtime/artifacts")).unwrap();
        std::fs::write(home.join("runtime/artifacts/big.bin"), b"never").unwrap();
        let db = rusqlite::Connection::open(home.join("cutex-sessions.sqlite3")).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE s(x); INSERT INTO s VALUES (1),(2);").unwrap();
        // The WAL is still open here: the snapshot must include its rows.
        std::mem::forget(db);
        home
    }

    #[test]
    fn backs_up_files_and_sqlite_snapshots() {
        let home = home();
        let dest = temp("dest");
        let manifest = backup(&home, &dest).unwrap();
        assert!(manifest.items.contains(&"config.json".to_string()));
        assert!(manifest.items.contains(&"runtime/asks".to_string()));
        assert!(!manifest.items.iter().any(|i| i.contains("artifacts")));
        let db = manifest.files.iter().find(|f| f.path == "cutex-sessions.sqlite3").unwrap();
        assert!(db.sqlite);
        let copy = rusqlite::Connection::open(dest.join("home/cutex-sessions.sqlite3")).unwrap();
        let rows: i64 = copy.query_row("SELECT count(*) FROM s", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 2);
        assert!(!manifest.files.iter().any(|f| f.path.ends_with("-wal") || f.path.ends_with("-shm")));
        assert_eq!(std::fs::read(dest.join("home/runtime/asks/sub/a.json")).unwrap(), b"ask");
        assert!(dest.join("home-manifest.json").is_file());
        std::fs::remove_dir_all(dest).unwrap();
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn selection_restore_and_move_aside_never_delete() {
        let home = home();
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
        for dir in [home, dest, displaced] {
            std::fs::remove_dir_all(dir).unwrap();
        }
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
