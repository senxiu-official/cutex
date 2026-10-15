//! One-time r219 migration (`archive/r219-removed/`, docs/r219-removals.md §4).
//!
//! r219 removed the legacy Task Service, Release rotation and the Director
//! seat copy. Their on-disk state is moved, never deleted, from
//! `<cutex>/runtime/…` to the same relative path under
//! `<cutex>/archive/r219-removed/`, at the start of `cutex management serve`
//! and of the agent bus server:
//!
//! - a missing source is a no-op, so every later start does nothing;
//! - an existing destination is never merged into or overwritten: the move
//!   goes to `<destination>.<UTC timestamp>` instead;
//! - a failed rename (on Windows: a file still open in an old r218 process)
//!   is logged and recorded but does not fail startup; the source is still
//!   there, so the next start retries it;
//! - `runtime/task-service/` itself and its `job-service-completion.token`
//!   stay (Job Service token, r219 agent bus host lock).
//!
//! Both daemons may start at once; `archive/r219-removed/.migration.lock`
//! serializes them. Runs that moved or failed something are appended to
//! `archive/r219-removed/migration.json`.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

pub const RECORD_SCHEMA: &str = "cutex/r219-removed-migration/v1";
/// Archive root, relative to the cutex dir.
const ARCHIVE_ROOT: &str = "archive/r219-removed";
const LOCK_FILE: &str = ".migration.lock";
const RECORD_FILE: &str = "migration.json";

/// `(step, source)`. Sources are relative to the cutex dir; each one moves to
/// the same relative path under [`ARCHIVE_ROOT`].
const STEPS: [(&str, &str); 4] = [
    // Task provider SQLite + WAL, watchdog / evidence stores, old bus host lock.
    ("task-worker-actions", "runtime/task-service/task-worker-actions-v1"),
    // Seat occupancy, Director seat transfers, release rotation receipts.
    ("task-service-seat-authority", "runtime/task-service/seat-authority-v1"),
    // Release template store.
    ("release-rotation", "runtime/release-rotation-v1"),
    // Seat copy kept next to an Agent Management store (non-default roots).
    (
        "agent-management-seat-authority",
        "runtime/agent-management/v1/task-service-seat-authority-v1",
    ),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum R219StepOutcome {
    Moved,
    Failed,
}

/// One step that moved or failed, as recorded in `migration.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct R219MigrationStep {
    pub step: String,
    /// Relative to the cutex dir, `/`-separated.
    pub source: String,
    /// Relative to the cutex dir, `/`-separated, with the collision suffix if
    /// one was needed.
    pub destination: String,
    pub outcome: R219StepOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// RFC 3339 UTC.
    pub time: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct R219MigrationReport {
    pub moved: Vec<R219MigrationStep>,
    pub failed: Vec<R219MigrationStep>,
    /// Steps whose source does not exist (already moved, or never created).
    pub absent: usize,
}

#[derive(Serialize, Deserialize)]
struct MigrationRecord {
    schema: String,
    /// Raw JSON, so runs written by a later release keep their fields.
    runs: Vec<serde_json::Value>,
}

#[derive(Serialize)]
struct MigrationRun<'a> {
    at: &'a str,
    steps: &'a [R219MigrationStep],
}

/// Daemon entry points call this first. Never fails or panics: an error only
/// means the migration is tried again on the next start.
pub fn run_on_startup(component: &str) {
    match std::panic::catch_unwind(run_once) {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            eprintln!("cutex r219 migration: {component}: not run on this start: {error:#}")
        }
        Err(_) => eprintln!("cutex r219 migration: {component}: panicked, not run on this start"),
    }
}

pub fn run_once() -> anyhow::Result<R219MigrationReport> {
    run_at(&crate::config::paths::config_dir()?, Utc::now())
}

/// Migrates the cutex dir `cutex_dir` (`~/.cutex`; its `runtime/` is
/// `paths::runtime_dir()`).
pub fn run_at(cutex_dir: &Path, now: DateTime<Utc>) -> anyhow::Result<R219MigrationReport> {
    let mut report = R219MigrationReport::default();
    // Almost every start has nothing to do: check before creating anything.
    if !STEPS.iter().any(|(_, source)| exists(&under(cutex_dir, source))) {
        report.absent = STEPS.len();
        return Ok(report);
    }
    let archive = under(cutex_dir, ARCHIVE_ROOT);
    ensure_private_dir(&cutex_dir.join("archive"))?;
    ensure_private_dir(&archive)?;
    let _lock = lock_exclusive(&archive.join(LOCK_FILE))?;
    // Read the record before moving anything: an unreadable record stops the
    // migration instead of losing its history.
    let record_path = archive.join(RECORD_FILE);
    let mut record = read_record(&record_path)?;
    let time = now.to_rfc3339_opts(SecondsFormat::Secs, true);
    let mut steps = Vec::new();
    for (step, source) in STEPS {
        // Re-checked under the lock: a concurrent start may have moved it.
        if !exists(&under(cutex_dir, source)) {
            report.absent += 1;
            continue;
        }
        steps.push(move_step(cutex_dir, step, source, now, &time));
    }
    if !steps.is_empty() {
        let run = MigrationRun { at: &time, steps: &steps };
        record.runs.push(serde_json::to_value(run)?);
        crate::config::atomic::write_private_pretty_json_atomic(
            &record_path,
            &record,
            "r219 migration record",
        )?;
    }
    for step in steps {
        match step.outcome {
            R219StepOutcome::Moved => report.moved.push(step),
            R219StepOutcome::Failed => report.failed.push(step),
        }
    }
    Ok(report)
}

fn move_step(
    cutex_dir: &Path,
    step: &str,
    source: &str,
    now: DateTime<Utc>,
    time: &str,
) -> R219MigrationStep {
    let destination = free_destination(cutex_dir, &format!("{ARCHIVE_ROOT}/{source}"), now);
    let target = under(cutex_dir, &destination);
    let result = target
        .parent()
        .map_or(Ok(()), ensure_private_dir)
        .and_then(|()| fs::rename(under(cutex_dir, source), &target).map_err(anyhow::Error::from));
    let error = match result {
        Ok(()) => {
            eprintln!("cutex r219 migration: moved {source} to {destination}");
            None
        }
        Err(error) => {
            let error = format!("{error:#}");
            eprintln!(
                "cutex r219 migration: could not move {source} to {destination} \
                 (retried on the next start): {error}"
            );
            Some(error)
        }
    };
    R219MigrationStep {
        step: step.to_owned(),
        source: source.to_owned(),
        destination,
        outcome: if error.is_none() { R219StepOutcome::Moved } else { R219StepOutcome::Failed },
        error,
        time: time.to_owned(),
    }
}

/// `wanted` if it is free, else `wanted.<UTC timestamp>` (then `-2`, `-3`, …
/// for several collisions within one second).
fn free_destination(cutex_dir: &Path, wanted: &str, now: DateTime<Utc>) -> String {
    if !exists(&under(cutex_dir, wanted)) {
        return wanted.to_owned();
    }
    let stamped = format!("{wanted}.{}", now.format("%Y%m%dT%H%M%SZ"));
    let mut candidate = stamped.clone();
    let mut counter = 1;
    while exists(&under(cutex_dir, &candidate)) {
        counter += 1;
        candidate = format!("{stamped}-{counter}");
    }
    candidate
}

fn read_record(path: &Path) -> anyhow::Result<MigrationRecord> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(MigrationRecord { schema: RECORD_SCHEMA.to_owned(), runs: Vec::new() })
        }
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let record: MigrationRecord =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    anyhow::ensure!(
        record.schema == RECORD_SCHEMA,
        "unexpected schema {:?} in {}",
        record.schema,
        path.display()
    );
    Ok(record)
}

fn lock_exclusive(path: &Path) -> anyhow::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let lock = options
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    fs2::FileExt::lock_exclusive(&lock).with_context(|| format!("lock {}", path.display()))?;
    Ok(lock)
}

/// Owner-private directory (Windows ACL / unix 0700), like other `~/.cutex`
/// children. Directories created below it inherit the protection.
fn ensure_private_dir(dir: &Path) -> anyhow::Result<()> {
    crate::management::client_tokens::ensure_private_dir(dir)
}

/// A broken link or an unreadable entry counts as present: the rename decides.
fn exists(path: &Path) -> bool {
    !matches!(
        fs::symlink_metadata(path),
        Err(error) if matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory)
    )
}

fn under(base: &Path, relative: &str) -> PathBuf {
    relative.split('/').fold(base.to_path_buf(), |path, part| path.join(part))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::Value;

    use super::*;

    const TOKEN: &str = "runtime/task-service/job-service-completion.token";
    const SQLITE: &str =
        "runtime/task-service/task-worker-actions-v1/task-service/provider-v2/task-service.sqlite3";

    struct TempHome(PathBuf);

    impl TempHome {
        fn new() -> Self {
            let dir = std::env::temp_dir()
                .join(format!("cutex-r219-migration-{}", uuid::Uuid::new_v4().simple()))
                .join(".cutex");
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(self.0.parent().unwrap());
        }
    }

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text).unwrap().with_timezone(&Utc)
    }

    fn write(dir: &Path, relative: &str, bytes: &[u8]) {
        let path = under(dir, relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    /// The four stores plus files that must stay.
    fn seed_all(dir: &Path) {
        write(dir, TOKEN, b"job-service-token");
        write(dir, SQLITE, b"sqlite main");
        write(dir, &format!("{SQLITE}-wal"), b"sqlite wal");
        write(dir, "runtime/task-service/task-worker-actions-v1/agent-bus-host.lock", b"");
        write(dir, "runtime/task-service/task-worker-actions-v1/evidence/e-1.json", b"{}");
        write(dir, "runtime/task-service/seat-authority-v1/seat-occupancy-v1.json", b"{\"seats\":[]}");
        write(dir, "runtime/release-rotation-v1/template/release-template-v1.json", b"{\"t\":1}");
        write(dir, "runtime/agent-management/v1/agent-management-v1.json", b"{\"am\":1}");
        write(
            dir,
            "runtime/agent-management/v1/task-service-seat-authority-v1/seat-occupancy-v1.json",
            b"{\"seats\":[1]}",
        );
    }

    /// Relative path (`/`) → bytes of every file below `root`.
    fn files(root: &Path) -> BTreeMap<String, Vec<u8>> {
        fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(root, &path, out);
                } else {
                    let relative = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                    out.insert(relative, fs::read(&path).unwrap());
                }
            }
        }
        let mut out = BTreeMap::new();
        if root.is_dir() {
            walk(root, root, &mut out);
        }
        out
    }

    fn record(dir: &Path) -> Value {
        let bytes = fs::read(under(dir, &format!("{ARCHIVE_ROOT}/{RECORD_FILE}"))).unwrap();
        let record: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(record["schema"], RECORD_SCHEMA);
        record
    }

    fn archived(source: &str) -> String {
        format!("{ARCHIVE_ROOT}/{source}")
    }

    #[test]
    fn r219_migration_moves_all_stores_and_keeps_the_token() {
        let home = TempHome::new();
        let dir = &home.0;
        seed_all(dir);
        let before: Vec<_> = STEPS.iter().map(|(_, source)| files(&under(dir, source))).collect();
        assert!(before.iter().all(|tree| !tree.is_empty()));

        let report = run_at(dir, at("2026-10-09T15:15:00Z")).unwrap();
        assert_eq!((report.moved.len(), report.failed.len(), report.absent), (4, 0, 0));
        for ((_, source), tree) in STEPS.iter().zip(before) {
            assert!(!under(dir, source).exists(), "{source} still there");
            assert_eq!(files(&under(dir, &archived(source))), tree, "{source}");
        }
        assert_eq!(fs::read(under(dir, TOKEN)).unwrap(), b"job-service-token");
        assert_eq!(
            files(&under(dir, "runtime/task-service")).into_keys().collect::<Vec<_>>(),
            ["job-service-completion.token"]
        );
        assert_eq!(
            fs::read(under(dir, "runtime/agent-management/v1/agent-management-v1.json")).unwrap(),
            b"{\"am\":1}"
        );

        let record = record(dir);
        let runs = record["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["at"], "2026-10-09T15:15:00Z");
        let steps = runs[0]["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 4);
        for (entry, (step, source)) in steps.iter().zip(STEPS) {
            assert_eq!(entry["step"], step);
            assert_eq!(entry["source"], source);
            assert_eq!(entry["destination"], archived(source));
            assert_eq!(entry["outcome"], "moved");
            assert_eq!(entry["time"], "2026-10-09T15:15:00Z");
            assert!(entry.get("error").is_none());
        }
    }

    #[test]
    fn r219_migration_second_run_is_a_no_op() {
        let home = TempHome::new();
        let dir = &home.0;
        seed_all(dir);
        run_at(dir, at("2026-10-09T15:15:00Z")).unwrap();
        let record_path = under(dir, &format!("{ARCHIVE_ROOT}/{RECORD_FILE}"));
        let record_before = fs::read(&record_path).unwrap();
        let archive_before = files(&under(dir, ARCHIVE_ROOT));

        let report = run_at(dir, at("2026-10-09T16:00:00Z")).unwrap();
        assert_eq!(report, R219MigrationReport { absent: 4, ..Default::default() });
        assert_eq!(fs::read(&record_path).unwrap(), record_before);
        assert_eq!(files(&under(dir, ARCHIVE_ROOT)), archive_before);
        assert_eq!(fs::read(under(dir, TOKEN)).unwrap(), b"job-service-token");
    }

    #[test]
    fn r219_migration_existing_destination_gets_a_timestamp_suffix() {
        let home = TempHome::new();
        let dir = &home.0;
        let source = "runtime/release-rotation-v1";
        write(dir, &format!("{}/template/old.json", archived(source)), b"old");
        write(dir, &format!("{source}/template/new.json"), b"new");

        let report = run_at(dir, at("2026-10-09T15:15:00Z")).unwrap();
        let stamped = format!("{}.20261009T151500Z", archived(source));
        assert_eq!(report.moved.len(), 1);
        assert_eq!(report.moved[0].destination, stamped);
        assert_eq!(fs::read(under(dir, &format!("{stamped}/template/new.json"))).unwrap(), b"new");
        assert_eq!(
            files(&under(dir, &archived(source))),
            BTreeMap::from([("template/old.json".to_owned(), b"old".to_vec())])
        );

        // A second collision within the same second gets a counter.
        write(dir, &format!("{source}/template/newer.json"), b"newer");
        let report = run_at(dir, at("2026-10-09T15:15:00Z")).unwrap();
        assert_eq!(report.moved[0].destination, format!("{stamped}-2"));
        assert_eq!(files(&under(dir, &stamped)).len(), 1);
        assert_eq!(record(dir)["runs"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn r219_migration_missing_sources_are_absent() {
        let home = TempHome::new();
        let dir = &home.0;
        write(dir, TOKEN, b"job-service-token");
        write(dir, "runtime/task-service/seat-authority-v1/seat-occupancy-v1.json", b"{}");

        let report = run_at(dir, at("2026-10-09T15:15:00Z")).unwrap();
        assert_eq!((report.moved.len(), report.failed.len(), report.absent), (1, 0, 3));
        assert_eq!(report.moved[0].step, "task-service-seat-authority");
        let steps = record(dir)["runs"][0]["steps"].as_array().unwrap().len();
        assert_eq!(steps, 1);
        assert_eq!(fs::read(under(dir, TOKEN)).unwrap(), b"job-service-token");
    }

    #[test]
    fn r219_migration_nothing_to_move_creates_nothing() {
        let home = TempHome::new();
        let dir = &home.0;
        write(dir, TOKEN, b"job-service-token");
        write(dir, "runtime/agent-management/v1/agent-management-v1.json", b"{}");

        let report = run_at(dir, at("2026-10-09T15:15:00Z")).unwrap();
        assert_eq!(report, R219MigrationReport { absent: 4, ..Default::default() });
        assert!(!dir.join("archive").exists());
        assert_eq!(files(&under(dir, "runtime/task-service")).len(), 1);
    }

    #[test]
    fn r219_migration_rejects_an_unreadable_record_before_moving() {
        let home = TempHome::new();
        let dir = &home.0;
        write(dir, "runtime/release-rotation-v1/template/t.json", b"{}");
        write(dir, &format!("{ARCHIVE_ROOT}/{RECORD_FILE}"), b"not json");

        assert!(run_at(dir, at("2026-10-09T15:15:00Z")).is_err());
        assert!(under(dir, "runtime/release-rotation-v1/template/t.json").is_file());
    }

    /// A file held open the way SQLite holds its database (no
    /// FILE_SHARE_DELETE) blocks renaming its directory on Windows.
    #[cfg(windows)]
    #[test]
    fn r219_migration_open_file_fails_one_step_and_is_retried() {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x1;
        const FILE_SHARE_WRITE: u32 = 0x2;

        let home = TempHome::new();
        let dir = &home.0;
        seed_all(dir);
        let held = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(under(dir, SQLITE))
            .unwrap();

        let report = run_at(dir, at("2026-10-09T15:15:00Z")).unwrap();
        assert_eq!((report.moved.len(), report.failed.len(), report.absent), (3, 1, 0));
        let failed = &report.failed[0];
        assert_eq!(failed.step, "task-worker-actions");
        assert_eq!(failed.destination, archived(STEPS[0].1));
        assert!(failed.error.as_deref().is_some_and(|error| !error.is_empty()));
        assert!(under(dir, SQLITE).is_file());
        assert!(!under(dir, &archived(STEPS[0].1)).exists());

        drop(held);
        let report = run_at(dir, at("2026-10-09T15:20:00Z")).unwrap();
        assert_eq!((report.moved.len(), report.failed.len(), report.absent), (1, 0, 3));
        assert_eq!(report.moved[0].destination, archived(STEPS[0].1));
        assert_eq!(fs::read(under(dir, &archived(SQLITE))).unwrap(), b"sqlite main");
        assert_eq!(fs::read(under(dir, TOKEN)).unwrap(), b"job-service-token");

        let record = record(dir);
        let runs = record["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 2);
        let first = runs[0]["steps"].as_array().unwrap();
        assert_eq!(first.len(), 4);
        assert_eq!(first[0]["outcome"], "failed");
        assert!(first[0]["error"].as_str().is_some_and(|error| !error.is_empty()));
        assert!(first[1..].iter().all(|step| step["outcome"] == "moved"));
        assert_eq!(runs[1]["steps"][0]["outcome"], "moved");
    }
}
