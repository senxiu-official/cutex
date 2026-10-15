//! Durable before-images and conservative recovery. A journal never stores
//! file contents in JSON, and unknown current hashes are never overwritten.
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use super::{
    content_hash, hash,
    plan::{self, Plan},
};

static EXECUTION: Mutex<()> = Mutex::new(());

pub struct ExecutionLock {
    _local: MutexGuard<'static, ()>,
    _file: File,
}

pub fn lock(home: &Path) -> Result<ExecutionLock, String> {
    let local = EXECUTION.lock().map_err(|_| "file revert lock poisoned")?;
    private_dir(home)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(home.join(".lock"))
        .map_err(|e| e.to_string())?;
    fs2::FileExt::lock_exclusive(&file).map_err(|e| e.to_string())?;
    Ok(ExecutionLock {
        _local: local,
        _file: file,
    })
}

#[derive(Debug)]
pub struct Failure {
    pub code: &'static str,
    pub message: String,
    pub journal: Option<PathBuf>,
    pub unknown: bool,
}
impl Failure {
    fn io(message: impl ToString, journal: Option<PathBuf>) -> Self {
        Self {
            code: "revert_rolled_back",
            message: message.to_string(),
            journal,
            unknown: false,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    path: PathBuf,
    before: Option<String>,
    after: Option<String>,
    backup: Option<String>,
    mode: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    version: u32,
    action_id: String,
    root: PathBuf,
    #[serde(default)]
    extra_root: Option<PathBuf>,
    digest: String,
    entries: Vec<Entry>,
    created_dirs: Vec<PathBuf>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct State {
    state: String,
    updated_at: chrono::DateTime<chrono::Utc>,
}

fn private_dir(path: &Path) -> Result<(), String> {
    crate::management::client_tokens::ensure_private_dir(path).map_err(|e| e.to_string())
}

fn state(dir: &Path, name: &str) -> Result<(), String> {
    crate::config::atomic::write_private_pretty_json_atomic(
        &dir.join("state.json"),
        &State {
            state: name.into(),
            updated_at: chrono::Utc::now(),
        },
        "file revert state",
    )
    .map_err(|e| e.to_string())
}

fn validate_path(record: &Record, path: &Path) -> Result<(), String> {
    let root = if path.starts_with(&record.root) {
        &record.root
    } else {
        record
            .extra_root
            .as_ref()
            .filter(|root| path.starts_with(root))
            .ok_or("journal path is outside allowed workspaces")?
    };
    let relative = path
        .strip_prefix(root)
        .map_err(|_| "journal path is outside root")?;
    let resolved = plan::resolve(root, &relative.to_string_lossy()).map_err(|(_, e)| e)?;
    if resolved != path {
        return Err("journal path changed".into());
    }
    Ok(())
}

fn current_hash(record: &Record, entry: &Entry) -> Result<Option<String>, String> {
    validate_path(record, &entry.path)?;
    Ok(content_hash(&plan::read(&entry.path)?))
}

fn replace(path: &Path, bytes: &Option<Vec<u8>>, mode: Option<u32>) -> Result<(), String> {
    match bytes {
        Some(bytes) => {
            let parent = path.parent().ok_or("path has no parent")?;
            let tmp = parent.join(format!(".cutex-frv-{}.tmp", uuid::Uuid::new_v4()));
            let result = (|| {
                let mut options = OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(mode.unwrap_or(0o600));
                }
                let mut file = options.open(&tmp).map_err(|e| e.to_string())?;
                file.write_all(bytes).map_err(|e| e.to_string())?;
                #[cfg(unix)]
                if let Some(mode) = mode {
                    use std::os::unix::fs::PermissionsExt;
                    file.set_permissions(fs::Permissions::from_mode(mode))
                        .map_err(|e| e.to_string())?;
                }
                #[cfg(not(unix))]
                let _ = mode;
                file.sync_all().map_err(|e| e.to_string())?;
                drop(file);
                fs::rename(&tmp, path).map_err(|e| e.to_string())?;
                sync_dir(parent)
            })();
            if result.is_err() {
                let _ = fs::remove_file(tmp);
            }
            result
        }
        None => {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.to_string()),
            }
            sync_dir(path.parent().ok_or("path has no parent")?)
        }
    }
}

fn sync_dir(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        File::open(path)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn backup(dir: &Path, entry: &Entry) -> Result<Option<Vec<u8>>, String> {
    let Some(name) = &entry.backup else {
        if entry.before.is_some() {
            return Err("missing before-image".into());
        }
        return Ok(None);
    };
    if name.parse::<usize>().is_err() {
        return Err("invalid backup name".into());
    }
    let path = dir.join("before").join(name);
    let bytes = plan::read(&path)?.ok_or("backup is missing")?;
    if Some(hash(&bytes)) != entry.before {
        return Err("backup hash mismatch".into());
    }
    Ok(Some(bytes))
}

fn restore(dir: &Path, record: &Record, count: usize, fail: bool) -> Result<(), String> {
    // Validate EVERY touched destination and backup before any rollback write.
    let mut backups = Vec::new();
    for entry in record.entries.iter().take(count) {
        let current = current_hash(record, entry)?;
        if current != entry.before && current != entry.after {
            return Err("current file has an unknown hash; manual recovery required".into());
        }
        backups.push(backup(dir, entry)?);
    }
    if fail {
        return Err("injected rollback failure".into());
    }
    for (entry, bytes) in record.entries.iter().take(count).zip(backups).rev() {
        // Repeat immediately before replacing, including during lazy recovery.
        let current = current_hash(record, entry)?;
        if current != entry.before && current != entry.after {
            return Err("file changed during rollback".into());
        }
        if current != entry.before {
            replace(&entry.path, &bytes, entry.mode)?;
        }
    }
    for dir in record.created_dirs.iter().rev() {
        validate_path(record, dir)?;
        match fs::remove_dir(dir) {
            Ok(()) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}

/// Requires ExecutionLock for the same journal home. No writes on digest mismatch.
pub fn execute(home: &Path, plan: &Plan, expected: &str) -> Result<(String, PathBuf), Failure> {
    execute_with(home, plan, expected, None, false, &mut || {})
}

fn execute_with(
    home: &Path,
    plan: &Plan,
    expected: &str,
    fail_at: Option<usize>,
    fail_rollback: bool,
    before_apply: &mut dyn FnMut(),
) -> Result<(String, PathBuf), Failure> {
    if plan.digest != expected {
        return Err(Failure {
            code: "plan_changed",
            message: "plan digest changed".into(),
            journal: None,
            unknown: false,
        });
    }
    if !plan.applicable() {
        return Err(Failure {
            code: if plan.blocked() {
                "revert_conflict"
            } else {
                "nothing_to_revert"
            },
            message: "plan cannot be applied".into(),
            journal: None,
            unknown: false,
        });
    }
    let action = format!("frv-{}", uuid::Uuid::new_v4());
    let dir = home.join(&action);
    private_dir(&dir).map_err(|e| Failure::io(e, None))?;
    private_dir(&dir.join("before")).map_err(|e| Failure::io(e, Some(dir.clone())))?;
    let mut record = Record {
        version: 1,
        action_id: action.clone(),
        root: plan.root.clone(),
        extra_root: plan.extra_root.clone(),
        digest: plan.digest.clone(),
        entries: Vec::new(),
        created_dirs: Vec::new(),
    };
    for (i, write) in plan.writes.iter().enumerate() {
        let name = write.before.as_ref().map(|_| i.to_string());
        if let Some(bytes) = &write.before {
            crate::config::atomic::write_private_bytes_atomic(
                &dir.join("before").join(i.to_string()),
                bytes,
            )
            .map_err(|e| Failure::io(e, Some(dir.clone())))?;
        }
        record.entries.push(Entry {
            path: write.path.clone(),
            before: content_hash(&write.before),
            after: content_hash(&write.after),
            backup: name,
            mode: write.mode,
        });
        if write.after.is_some() {
            let mut missing = Vec::new();
            let mut parent = write.path.parent();
            while let Some(path) = parent {
                if path.exists() {
                    break;
                }
                missing.push(path.to_owned());
                parent = path.parent();
            }
            for path in missing.into_iter().rev() {
                if !record.created_dirs.contains(&path) {
                    record.created_dirs.push(path);
                }
            }
        }
    }
    crate::config::atomic::write_private_pretty_json_atomic(
        &dir.join("plan.json"),
        &record,
        "file revert plan",
    )
    .map_err(|e| Failure::io(e, Some(dir.clone())))?;
    state(&dir, "prepared").map_err(|e| Failure::io(e, Some(dir.clone())))?;
    // All preconditions are checked before the first workspace mutation.
    for entry in &record.entries {
        if current_hash(&record, entry).ok().as_ref() != Some(&entry.before) {
            return Err(Failure {
                code: "plan_changed",
                message: "file changed while preparing journal".into(),
                journal: Some(dir),
                unknown: false,
            });
        }
    }
    state(&dir, "applying").map_err(|e| Failure::io(e, Some(dir.clone())))?;
    let mut touched = 0;
    let mut changed = false;
    before_apply();
    let applied: Result<(), String> = (|| {
        for path in &record.created_dirs {
            validate_path(&record, path)?;
            fs::create_dir(path).map_err(|e| e.to_string())?;
        }
        for (i, (entry, write)) in record.entries.iter().zip(&plan.writes).enumerate() {
            if current_hash(&record, entry)? != entry.before {
                changed = true;
                return Err("file changed immediately before write".into());
            }
            if fail_at == Some(i) {
                return Err("injected write failure".into());
            }
            // Include this entry even if rename succeeded but syncing failed.
            touched = i + 1;
            replace(&entry.path, &write.after, entry.mode)?;
        }
        for entry in &record.entries {
            if current_hash(&record, entry)? != entry.after {
                return Err("file changed before commit".into());
            }
        }
        state(&dir, "applied")
    })();
    if let Err(error) = applied {
        match restore(&dir, &record, touched, fail_rollback).and_then(|_| state(&dir, "rolledBack"))
        {
            Ok(()) => {
                return Err(Failure {
                    code: if changed {
                        "plan_changed"
                    } else {
                        "revert_rolled_back"
                    },
                    message: error,
                    journal: Some(dir),
                    unknown: false,
                })
            }
            Err(rollback) => {
                let _ = state(&dir, "rollbackFailed");
                return Err(Failure {
                    code: "revert_partial",
                    message: format!("{error}; {rollback}"),
                    journal: Some(dir),
                    unknown: true,
                });
            }
        }
    }
    Ok((action, dir))
}

/// Must run under ExecutionLock before a new execution. Unknown states block
/// new writes until an owner repairs the journal; dry-runs remain read-only.
pub fn recover(home: &Path) -> Result<Vec<String>, Failure> {
    let mut recovered = Vec::new();
    for entry in fs::read_dir(home).map_err(|e| Failure::io(e, None))? {
        let entry = entry.map_err(|e| Failure::io(e, None))?;
        if !entry.file_name().to_string_lossy().starts_with("frv-") {
            continue;
        }
        let dir = entry.path();
        if !entry
            .file_type()
            .map_err(|e| Failure::io(e, None))?
            .is_dir()
        {
            continue;
        }
        let bytes = match fs::read(dir.join("state.json")) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue, // preparation never reached its first state
            Err(error) => {
                return Err(Failure {
                    code: "revert_partial",
                    message: error.to_string(),
                    journal: Some(dir),
                    unknown: true,
                })
            }
        };
        let status: State = serde_json::from_slice(&bytes).map_err(|error| Failure {
            code: "revert_partial",
            message: format!("journal state is unreadable: {error}"),
            journal: Some(dir.clone()),
            unknown: true,
        })?;
        if !matches!(
            status.state.as_str(),
            "prepared" | "applying" | "applied" | "rolledBack" | "rollbackFailed"
        ) {
            return Err(Failure {
                code: "revert_partial",
                message: "unknown journal state".into(),
                journal: Some(dir),
                unknown: true,
            });
        }
        if status.state == "rollbackFailed" {
            return Err(Failure {
                code: "revert_partial",
                message: "an earlier rollback needs manual recovery".into(),
                journal: Some(dir),
                unknown: true,
            });
        }
        if status.state == "applying" {
            let result = (|| -> Result<String, String> {
                let record: Record = serde_json::from_slice(
                    &fs::read(dir.join("plan.json")).map_err(|e| e.to_string())?,
                )
                .map_err(|e| e.to_string())?;
                if record.version != 1
                    || dir.file_name().and_then(|n| n.to_str()) != Some(&record.action_id)
                {
                    return Err("invalid journal version or action".into());
                }
                restore(&dir, &record, record.entries.len(), false)?;
                state(&dir, "rolledBack")?;
                Ok(record.action_id)
            })();
            match result {
                Ok(id) => recovered.push(id),
                Err(error) => {
                    let _ = state(&dir, "rollbackFailed");
                    return Err(Failure {
                        code: "revert_partial",
                        message: error,
                        journal: Some(dir),
                        unknown: true,
                    });
                }
            }
        } else if matches!(status.state.as_str(), "applied" | "rolledBack")
            && chrono::Utc::now()
                .signed_duration_since(status.updated_at)
                .num_days()
                >= 7
        {
            // Only an immediate, ordinary directory from this verified home.
            fs::remove_dir_all(&dir).map_err(|e| Failure::io(e, Some(dir)))?;
        }
    }
    Ok(recovered)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup() -> (PathBuf, PathBuf, Plan) {
        let root =
            std::env::temp_dir().join(format!("cutex-files-journal-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let home = root.join("journal");
        private_dir(&home).unwrap();
        let mut changes = Vec::new();
        for path in ["a", "b"] {
            fs::write(root.join(path), "new\n").unwrap();
            changes.push(plan::Change {
                path: path.into(),
                kind: "update".into(),
                move_path: None,
                diff: "-old\n+new\n".into(),
            });
        }
        let plan = plan::build(&root, &changes, None).unwrap();
        (root, home, plan)
    }
    /// NTFS keeps the spelling a rename gives: restoring `README.md` through a
    /// lowercased path would rename it. Names keep their on-disk / history case.
    #[cfg(windows)]
    #[test]
    fn windows_restore_and_recreate_keep_file_name_case() {
        let root =
            std::env::temp_dir().join(format!("cutex-files-case-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("Src")).unwrap();
        let home = root.join("journal");
        private_dir(&home).unwrap();
        fs::write(root.join("Src").join("README.md"), "after
").unwrap();
        let change = |path: &str, kind: &str, diff: &str| plan::Change {
            path: path.into(),
            kind: kind.into(),
            move_path: None,
            diff: diff.into(),
        };
        let changes = [
            change("src/readme.MD", "update", "@@ -1 +1 @@
-before
+after
"),
            change("Docs/Guide.MD", "delete", "guide
"),
            change("New.txt", "add", "n
"),
            change("NEW.TXT", "update", "@@ -1 +1 @@
-n
+m
"),
        ];
        fs::write(root.join("New.txt"), "m
").unwrap();
        let plan = plan::build(&root, &changes, None).unwrap();
        assert!(plan.applicable(), "{:?}", plan.files);
        assert_eq!(plan.files.len(), 3);
        execute(&home, &plan, &plan.digest.clone()).unwrap();
        let names = |dir: &Path| -> Vec<String> {
            let mut names: Vec<_> = fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };
        assert_eq!(names(&root.join("Src")), ["README.md"]);
        assert_eq!(fs::read_to_string(root.join("Src").join("README.md")).unwrap(), "before
");
        assert_eq!(names(&root.join("Docs")), ["Guide.MD"]);
        assert!(names(&root).contains(&"Docs".to_owned()));
        assert!(!names(&root).iter().any(|n| n.eq_ignore_ascii_case("new.txt")));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn digest_and_recheck_never_clobber_new_content() {
        let (root, home, plan) = setup();
        assert_eq!(
            execute(&home, &plan, "bad").unwrap_err().code,
            "plan_changed"
        );
        let error = execute_with(&home, &plan, &plan.digest, None, false, &mut || {
            fs::write(root.join("a"), "external").unwrap();
        })
        .unwrap_err();
        assert_eq!(error.code, "plan_changed");
        assert_eq!(fs::read_to_string(root.join("a")).unwrap(), "external");
        assert_eq!(fs::read_to_string(root.join("b")).unwrap(), "new\n");
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn write_failure_rolls_back_and_rollback_failure_is_durable() {
        for rollback in [false, true] {
            let (root, home, plan) = setup();
            let error = execute_with(&home, &plan, &plan.digest, Some(1), rollback, &mut || {})
                .unwrap_err();
            assert_eq!(
                error.code,
                if rollback {
                    "revert_partial"
                } else {
                    "revert_rolled_back"
                }
            );
            assert_eq!(error.unknown, rollback);
            if !rollback {
                assert_eq!(fs::read_to_string(root.join("a")).unwrap(), "new\n");
            }
            fs::remove_dir_all(root).unwrap();
        }
    }
    #[test]
    fn lazy_recovery_known_unknown_and_retention() {
        for unknown in [false, true] {
            let (root, home, plan) = setup();
            let (_, dir) = execute(&home, &plan, &plan.digest).unwrap();
            state(&dir, "applying").unwrap();
            if unknown {
                fs::write(root.join("a"), "external").unwrap();
            }
            let result = recover(&home);
            if unknown {
                assert_eq!(result.unwrap_err().code, "revert_partial");
                assert_eq!(fs::read_to_string(root.join("b")).unwrap(), "old\n");
            } else {
                assert_eq!(result.unwrap().len(), 1);
                assert_eq!(fs::read_to_string(root.join("a")).unwrap(), "new\n");
                crate::config::atomic::write_private_pretty_json_atomic(
                    &dir.join("state.json"),
                    &State {
                        state: "rolledBack".into(),
                        updated_at: chrono::Utc::now() - chrono::Duration::days(8),
                    },
                    "test",
                )
                .unwrap();
                recover(&home).unwrap();
                assert!(!dir.exists());
            }
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn rollback_removes_created_files_and_empty_parent_directories() {
        let (root, home, _) = setup();
        let changes = [
            plan::Change {
                path: "nested/deeper/a".into(),
                kind: "delete".into(),
                move_path: None,
                diff: "restore me\n".into(),
            },
            plan::Change {
                path: "z".into(),
                kind: "delete".into(),
                move_path: None,
                diff: "second\n".into(),
            },
        ];
        let plan = plan::build(&root, &changes, None).unwrap();
        let failure =
            execute_with(&home, &plan, &plan.digest, Some(1), false, &mut || {}).unwrap_err();
        assert_eq!(failure.code, "revert_rolled_back");
        assert!(!root.join("nested").exists());
        assert!(!root.join("z").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corrupted_before_image_blocks_all_recovery_writes() {
        let (root, home, plan) = setup();
        let (_, dir) = execute(&home, &plan, &plan.digest).unwrap();
        fs::write(dir.join("before/1"), "corrupted").unwrap();
        state(&dir, "applying").unwrap();
        assert_eq!(recover(&home).unwrap_err().code, "revert_partial");
        for path in ["a", "b"] {
            assert_eq!(fs::read_to_string(root.join(path)).unwrap(), "old\n");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unix_file_modes_survive_replace_and_move_back() {
        use std::os::unix::fs::PermissionsExt;
        let (root, home, _) = setup();
        fs::set_permissions(root.join("a"), fs::Permissions::from_mode(0o751)).unwrap();
        let changes = [plan::Change {
            path: "original".into(),
            move_path: Some("a".into()),
            kind: "update".into(),
            diff: "-old\n+new\n".into(),
        }];
        let plan = plan::build(&root, &changes, None).unwrap();
        execute(&home, &plan, &plan.digest).unwrap();
        assert_eq!(
            fs::metadata(root.join("original"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o751
        );
        fs::remove_dir_all(root).unwrap();
    }
}
