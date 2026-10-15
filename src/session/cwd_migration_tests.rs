use std::path::PathBuf;

use serde_json::Value;

use super::*;

/// Private temp cutex dir (never the real home: the test home guard refuses
/// it) with the session store at its usual logical path.
struct TempHome {
    root: PathBuf,
    dir: PathBuf,
}

impl TempHome {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "cutex-cwd-migration-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let dir = root.join(".cutex");
        fs::create_dir_all(&dir).unwrap();
        Self { root, dir }
    }

    fn store(&self) -> PathBuf {
        self.dir.join("cutex-sessions.json")
    }

    fn record_path(&self) -> PathBuf {
        self.dir.join(ARCHIVE_ROOT).join(RECORD_FILE)
    }

    fn seed(&self, records: Vec<CutexSessionRecord>) {
        let path = self.store();
        crate::session::store::with_locked_session_store(&path, |store| {
            for record in records {
                store.sessions.insert(record.cutex_session_id.clone(), record);
            }
            crate::session::store::save_locked_session_store(&path, store)
        })
        .unwrap();
    }

    fn load(&self) -> CutexSessionStore {
        crate::session::store::load_cutex_session_store_from_path(&self.store()).unwrap()
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn at(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text).unwrap().with_timezone(&Utc)
}

/// A record whose stored cwd is `cwd` exactly (bypassing the input
/// normalization of `new_at`, like a record written by an older release).
fn record(name: &str, cwd: &str) -> CutexSessionRecord {
    let mut record = CutexSessionRecord::new_at(
        format!("cutex.{name}"),
        None,
        "host".to_string(),
        "/placeholder".to_string(),
        None,
        "2026-10-01T00:00:00Z".to_string(),
    )
    .unwrap();
    record.cwd = cwd.to_string();
    record.thread_name = Some(name.to_string());
    record.display_name_hint = Some(name.to_string());
    record
}

/// `E:\a\b` -> `E:\\a\\b` (two literal backslashes per separator).
#[cfg(windows)]
fn double_escaped(path: &str) -> String {
    path.replace('\\', "\\\\")
}

const MISSING_BAD: &str = r"Q:\\cutex-cwd-migration-missing\\waveline-backend";
const MISSING_GOOD: &str = r"Q:\cutex-cwd-migration-missing\waveline-backend";

#[test]
fn cwd_migration_fixes_missing_paths_and_is_idempotent() {
    let home = TempHome::new();
    let mut bad = record("staff-backend-1", MISSING_BAD);
    let old_group = default_group(MISSING_BAD);
    bad.agent_groups = vec![old_group.clone(), "waveline".to_string()];
    let bad_revision = bad.durable_revision();
    let mut managed = record("managed", "/home/u/x");
    managed.managed_cwd = Some(MISSING_BAD.to_string());
    let clean = record("clean", MISSING_GOOD);
    let posix = record("posix", "/home/u//odd/");
    home.seed(vec![bad, managed, clean.clone(), posix.clone()]);

    let changes = run_at(&home.dir, &home.store(), "test", at("2026-10-11T01:02:03Z")).unwrap();
    assert_eq!(changes.len(), 2);
    let store = home.load();

    let fixed = &store.sessions["cutex.staff-backend-1"];
    assert_eq!(fixed.cwd, MISSING_GOOD);
    assert_eq!(fixed.agent_groups, vec![default_group(MISSING_GOOD), "waveline".to_string()]);
    assert_eq!(fixed.durable_revision(), bad_revision + 1);
    assert_eq!(fixed.updated_at, "2026-10-11T01:02:03Z");

    let managed = &store.sessions["cutex.managed"];
    assert_eq!(managed.managed_cwd.as_deref(), Some(MISSING_GOOD));
    assert_eq!(managed.cwd, "/home/u/x");

    // Clean and POSIX records are untouched (not even their revision).
    assert_eq!(store.sessions["cutex.clean"], clean);
    assert_eq!(store.sessions["cutex.posix"], posix);

    // The record holds old and new values (the backup).
    let record: Value = serde_json::from_slice(&fs::read(home.record_path()).unwrap()).unwrap();
    assert_eq!(record["schema"], RECORD_SCHEMA);
    let runs = record["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["at"], "2026-10-11T01:02:03Z");
    assert_eq!(runs[0]["component"], "test");
    assert_eq!(runs[0]["committed"], true);
    let sessions = runs[0]["sessions"].as_array().unwrap();
    let staff = sessions
        .iter()
        .find(|session| session["key"] == "cutex.staff-backend-1")
        .unwrap();
    assert_eq!(staff["name"], "staff-backend-1");
    assert_eq!(staff["cwd"]["old"], MISSING_BAD);
    assert_eq!(staff["cwd"]["new"], MISSING_GOOD);
    assert_eq!(staff["groups"]["old"][0], old_group);
    assert_eq!(staff["previousRevision"], bad_revision);
    assert_eq!(staff["previousUpdatedAt"], "2026-10-01T00:00:00Z");
    let managed = sessions.iter().find(|session| session["key"] == "cutex.managed").unwrap();
    assert!(managed.get("cwd").is_none());
    assert_eq!(managed["managedCwd"]["old"], MISSING_BAD);

    // Second start: nothing to do, nothing written.
    let record_bytes = fs::read(home.record_path()).unwrap();
    let revision = home.load().store_revision.get();
    let again = run_at(&home.dir, &home.store(), "test", at("2026-10-11T02:00:00Z")).unwrap();
    assert!(again.is_empty());
    assert_eq!(fs::read(home.record_path()).unwrap(), record_bytes);
    assert_eq!(home.load().store_revision.get(), revision);
}

#[test]
fn cwd_migration_with_nothing_to_do_creates_nothing() {
    let home = TempHome::new();
    home.seed(vec![record("clean", MISSING_GOOD), record("posix", "/tmp/x")]);
    let revision = home.load().store_revision.get();
    let changes = run_at(&home.dir, &home.store(), "test", at("2026-10-11T01:00:00Z")).unwrap();
    assert!(changes.is_empty());
    assert!(!home.dir.join("archive").exists());
    assert_eq!(home.load().store_revision.get(), revision);
}

#[test]
fn cwd_migration_on_a_home_without_a_store_creates_nothing() {
    let home = TempHome::new();
    let changes = run_at(&home.dir, &home.store(), "test", at("2026-10-11T01:00:00Z")).unwrap();
    assert!(changes.is_empty());
    assert!(!home.dir.join("archive").exists());
    assert!(!home.store().exists());
}

#[test]
fn cwd_migration_rejects_an_unreadable_record_before_changing_the_store() {
    let home = TempHome::new();
    home.seed(vec![record("bad", MISSING_BAD)]);
    fs::create_dir_all(home.record_path().parent().unwrap()).unwrap();
    fs::write(home.record_path(), b"not json").unwrap();
    assert!(run_at(&home.dir, &home.store(), "test", at("2026-10-11T01:00:00Z")).is_err());
    assert_eq!(home.load().sessions["cutex.bad"].cwd, MISSING_BAD);
}

#[test]
fn cwd_migration_keeps_unrelated_groups_and_dedupes() {
    let old = default_group(MISSING_BAD);
    let new = default_group(MISSING_GOOD);
    let groups = vec![new.clone(), old.clone(), "project:a0e7fe3f".to_string()];
    let change = renamed_default_group(&groups, MISSING_BAD, MISSING_GOOD).unwrap();
    assert_eq!(change.new, vec![new, "project:a0e7fe3f".to_string()]);
    assert!(renamed_default_group(&["waveline".to_string()], MISSING_BAD, MISSING_GOOD).is_none());
    assert!(renamed_default_group(&[old], MISSING_GOOD, MISSING_GOOD).is_none());
}

/// The Windows-host shape: the double-escaped spelling of a directory that exists
/// (Windows resolves both spellings to it).
#[cfg(windows)]
#[test]
fn cwd_migration_fixes_an_existing_double_escaped_directory() {
    let home = TempHome::new();
    let project = home.root.join("Projects (Orchid)").join("waveline-backend");
    fs::create_dir_all(&project).unwrap();
    let good = normalize_session_cwd(&project.display().to_string());
    let bad = double_escaped(&good);
    assert!(Path::new(&bad).exists(), "Windows resolves the escaped spelling too");
    home.seed(vec![
        record("staff-backend-1", &bad),
        record("staff-backend-2", &bad),
        record("staff-backend-3", &bad),
        record("orchid-1", &good),
    ]);

    let changes = run_at(&home.dir, &home.store(), "test", at("2026-10-11T01:00:00Z")).unwrap();
    let mut names: Vec<_> = changes.iter().map(|change| change.name.as_str()).collect();
    names.sort();
    assert_eq!(names, ["staff-backend-1", "staff-backend-2", "staff-backend-3"]);
    let store = home.load();
    for name in ["staff-backend-1", "staff-backend-2", "staff-backend-3", "orchid-1"] {
        assert_eq!(store.sessions[&format!("cutex.{name}")].cwd, good, "{name}");
    }
}

