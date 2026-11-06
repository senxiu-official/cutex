use std::collections::HashSet;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;

use anyhow::Context;
use chrono::Datelike;
use fs2::FileExt;

use crate::config::atomic::write_private_pretty_json_atomic;

use super::model::parse_timestamp;
use super::model::UsageLedger;
use super::model::UsageLedgerEntry;
use super::model::UsageStateFile;
use super::model::UsageStateSnapshot;
use super::index::IndexReader;
use super::index::LedgerFileKey;
use super::index::ReadStats;
use super::index::SampleFilter;
use super::index::SyncStats;
use super::index::UsageIndex;

const USAGE_STATE_FILE: &str = "session-usage-state.json";
const USAGE_LOCK_FILE: &str = "session-usage-state.lock";
pub(super) const USAGE_LEDGER_DIR: &str = "usage-ledger";
/// Rows imported from extra usage sources (`external.rs`): one directory per
/// source id, same `YYYY-MM.jsonl` layout. Removing a source's directory
/// purges exactly its rows.
pub(super) const USAGE_EXTERNAL_DIR: &str = "usage-ledger-external";

static USAGE_STORE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
/// Roots whose index this process synced with start fingerprints compared.
static INDEX_VERIFIED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

#[derive(Debug, Default)]
pub(super) struct UsageMutation {
    pub changed: bool,
    pub entries: Vec<UsageLedgerEntry>,
}

pub(super) fn forget_deleted_session_at(root: &Path, id: &str) -> anyhow::Result<()> {
    with_usage_store_lock(root, |state_path, ledger_dir| {
        let mut state = load_usage_state(state_path)?;
        flush_pending_entries(state_path, ledger_dir, &mut state)?;
        let removed = state.sessions.remove(id).is_some();
        if state.deleted_sessions.insert(id.to_string()) || removed {
            state.mark_changed(&chrono::Utc::now().to_rfc3339())?;
            write_usage_state(state_path, &state)?;
        }
        Ok(())
    })
}

pub(super) fn update_usage_state_at(
    root: &Path,
    received_at: &str,
    mutate: impl FnOnce(&mut UsageStateFile) -> anyhow::Result<UsageMutation>,
) -> anyhow::Result<()> {
    with_usage_store_lock(root, |state_path, ledger_dir| {
        let mut state = load_usage_state(state_path)?;
        flush_pending_entries(state_path, ledger_dir, &mut state)?;

        let mutation = mutate(&mut state)?;
        if !mutation.changed {
            return Ok(());
        }
        state.mark_changed(received_at)?;
        state.pending_entries = mutation.entries;
        state.validate()?;
        write_usage_state(state_path, &state)?;
        flush_pending_entries(state_path, ledger_dir, &mut state)
    })
}

pub(super) fn load_usage_state_snapshot_at(root: &Path) -> anyhow::Result<UsageStateSnapshot> {
    with_usage_store_lock(root, |state_path, ledger_dir| {
        let mut state = load_usage_state(state_path)?;
        flush_pending_entries(state_path, ledger_dir, &mut state)?;
        Ok(state.snapshot())
    })
}

/// Whole own ledger (both kinds), from the index. CLI report / TUI.
pub(super) fn load_usage_ledger_at(root: &Path) -> anyhow::Result<UsageLedger> {
    Ok(load_usage_data_at(root)?.1)
}

pub(super) fn load_usage_data_at(root: &Path) -> anyhow::Result<(UsageStateSnapshot, UsageLedger)> {
    let (state, (ledger, _)) = read_index(root, &|source| source.is_empty(), None, |reader| {
        let mut ledger = UsageLedger { warnings: reader.warnings(None)?, ..UsageLedger::default() };
        reader.for_each_entry(|entry| {
            ledger.entries.push(entry);
            Ok(())
        })?;
        Ok(ledger)
    })?;
    Ok((state, ledger))
}

/// Usage samples selected by `filter` from cutex's own ledger and every
/// extra source not in `hidden`, in ledger order, each decoded once and
/// handed to `visit`. Returns the warnings of invalid lines in the files of
/// months `[first_month, last_month]` (as the month reader reported them)
/// and the work counters.
pub(super) fn read_usage_samples_at(
    root: &Path,
    months: (&str, &str),
    hidden: &HashSet<String>,
    filter: &SampleFilter,
    visit: impl FnMut(super::model::UsageSample) -> anyhow::Result<()>,
) -> anyhow::Result<(Vec<String>, ReadStats)> {
    let visible = |source: &str| source.is_empty() || !hidden.contains(source);
    let (_, read) = read_index(root, &visible, Some(months), |reader| {
        let warnings = reader.warnings(Some(months))?;
        reader.for_each_sample(filter, visit)?;
        Ok(warnings)
    })?;
    Ok(read)
}

/// The ledger as the month reader returned it (entries of both kinds,
/// warnings), materialized from the index. Verification and tests only:
/// report paths stream through [`read_usage_samples_at`].
pub(super) fn load_usage_ledger_months_at(
    root: &Path,
    first_month: &str,
    last_month: &str,
    hidden: &HashSet<String>,
) -> anyhow::Result<UsageLedger> {
    let visible = |source: &str| source.is_empty() || !hidden.contains(source);
    let months = (first_month, last_month);
    let (_, (ledger, _)) = read_index(root, &visible, Some(months), |reader| {
        let mut ledger = UsageLedger { warnings: reader.warnings(Some(months))?, ..UsageLedger::default() };
        reader.for_each_entry(|entry| {
            ledger.entries.push(entry);
            Ok(())
        })?;
        Ok(ledger)
    })?;
    Ok(ledger)
}

/// Under the writer lock: flush pending entries and bring the index up to
/// the ledger files; [`UsageIndex::read`] pins its snapshot and opens the
/// visible files, then releases the lock. Decoding runs outside it.
fn read_index<T>(
    root: &Path,
    visible: &dyn Fn(&str) -> bool,
    months: Option<(&str, &str)>,
    read: impl FnOnce(&mut IndexReader<'_>) -> anyhow::Result<T>,
) -> anyhow::Result<(UsageStateSnapshot, (T, ReadStats))> {
    let guard = UsageStoreGuard::lock(root)?;
    let state_path = root.join(USAGE_STATE_FILE);
    let mut state = load_usage_state(&state_path)?;
    flush_pending_entries(&state_path, &root.join(USAGE_LEDGER_DIR), &mut state)?;
    let (index, _) = synced_index(root, false)?;
    let read = index.read(root, guard, visible, months, read)?;
    Ok((state.snapshot(), read))
}

/// Open and catch up the index (caller holds the usage lock). Start
/// fingerprints are compared once per process and root (`always_verify`:
/// every time), which catches a file replaced at the same length by an
/// older binary.
fn synced_index(root: &Path, always_verify: bool) -> anyhow::Result<(UsageIndex, SyncStats)> {
    let verified = INDEX_VERIFIED.get_or_init(Default::default);
    let verify = always_verify
        || !verified.lock().map_err(|_| anyhow::anyhow!("usage index lock poisoned"))?.contains(root);
    let index = UsageIndex::open(root)?;
    let stats = index.sync(root, verify)?;
    if verify {
        verified.lock().map_err(|_| anyhow::anyhow!("usage index lock poisoned"))?.insert(root.to_path_buf());
    }
    Ok((index, stats))
}

/// Build or catch up the index of `root` now (kit migration / repair).
pub(super) fn sync_usage_index_at(root: &Path) -> anyhow::Result<SyncStats> {
    with_usage_store_lock(root, |state_path, ledger_dir| {
        let mut state = load_usage_state(state_path)?;
        flush_pending_entries(state_path, ledger_dir, &mut state)?;
        Ok(synced_index(root, true)?.1)
    })
}

/// The pre-index reader (058 S1): parses every line of the month files of
/// cutex's own ledger and, with `hidden`, every extra source not in it. Kept
/// as the oracle the index is verified against (kit verify, tests); no
/// report path uses it.
pub(super) fn legacy_ledger_at(
    root: &Path,
    months: Option<(&str, &str)>,
    hidden: Option<&HashSet<String>>,
) -> anyhow::Result<UsageLedger> {
    let files = with_usage_store_lock(root, |state_path, ledger_dir| {
        let mut state = load_usage_state(state_path)?;
        flush_pending_entries(state_path, ledger_dir, &mut state)?;
        let mut dirs = vec![ledger_dir.to_path_buf()];
        if let Some(hidden) = hidden {
            dirs.extend(external_source_dirs(root, hidden)?);
        }
        capture_ledger_files(&dirs, |stem| {
            months.is_none_or(|(first, last)| stem.len() == 7 && stem >= first && stem <= last)
        })
    })?;
    read_captured_ledger(files)
}

/// Result of [`verify_usage_index_at`].
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageIndexVerification {
    /// Entries / warnings compared, all sources visible.
    pub entries: u64,
    pub warnings: u64,
    /// Entries compared, cutex's own ledger only (CLI report view).
    pub own_entries: u64,
}

/// Compare what the index yields with the old reader, entry by entry and in
/// order, for the whole ledger with every source visible and for cutex's
/// own ledger alone. Holds one legacy copy at a time.
pub(super) fn verify_usage_index_at(root: &Path) -> anyhow::Result<UsageIndexVerification> {
    let mut result = UsageIndexVerification { entries: 0, warnings: 0, own_entries: 0 };
    for own_only in [false, true] {
        let none = HashSet::new();
        let legacy = legacy_ledger_at(root, None, (!own_only).then_some(&none))?;
        let visible = |source: &str| !own_only || source.is_empty();
        let (_, (warnings, _)) = read_index(root, &visible, None, |reader| {
            let warnings = reader.warnings(None)?;
            anyhow::ensure!(
                warnings == legacy.warnings,
                "usage index warnings differ from the ledger ({} vs {})",
                warnings.len(),
                legacy.warnings.len()
            );
            let mut expected = legacy.entries.iter();
            let mut compared = 0_u64;
            reader.for_each_entry(|entry| {
                let Some(old) = expected.next() else {
                    anyhow::bail!("usage index has more entries than the ledger (at {})", entry.entry_id());
                };
                anyhow::ensure!(
                    *old == entry,
                    "usage index entry {compared} differs from the ledger: {} vs {}",
                    entry.entry_id(),
                    old.entry_id()
                );
                compared += 1;
                Ok(())
            })?;
            anyhow::ensure!(
                expected.next().is_none(),
                "usage index has fewer entries than the ledger ({compared} of {})",
                legacy.entries.len()
            );
            Ok((warnings.len() as u64, compared))
        })?;
        let (warnings, compared) = warnings;
        if own_only {
            result.own_entries = compared;
        } else {
            result.entries = compared;
            result.warnings = warnings;
        }
    }
    Ok(result)
}

/// `usage-ledger-external/<id>` of every source id not in `hidden`, sorted.
fn external_source_dirs(root: &Path, hidden: &HashSet<String>) -> anyhow::Result<Vec<PathBuf>> {
    let base = root.join(USAGE_EXTERNAL_DIR);
    let entries = match fs::read_dir(&base) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to read usage ledger: {}", base.display()))
        }
    };
    let mut dirs = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| !hidden.contains(name))
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    dirs.sort();
    Ok(dirs)
}

/// Append rows of one extra usage source (validated; same torn-write
/// framing as cutex's own ledger). Holds the usage store lock, so readers
/// never see a partial batch.
pub(super) fn append_external_entries(
    root: &Path,
    source_id: &str,
    entries: &[UsageLedgerEntry],
) -> anyhow::Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    with_usage_store_lock(root, |_, _| {
        let base = root.join(USAGE_EXTERNAL_DIR);
        fs::create_dir_all(&base)?;
        secure_directory(&base)?;
        let dir = base.join(source_id);
        let mut months = Vec::new();
        for entry in entries {
            months.push(append_ledger_entry(&dir, entry)?);
        }
        index_appended(root, source_id, months);
        Ok(())
    })
}

/// Delete every row (and the scan state) of one extra usage source.
pub(super) fn purge_external_source(root: &Path, source_id: &str) -> anyhow::Result<bool> {
    with_usage_store_lock(root, |_, _| {
        super::external::forget_checkpoints_at(root,source_id)?;
        let dir = root.join(USAGE_EXTERNAL_DIR).join(source_id);
        let removed = match fs::remove_dir_all(&dir) {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Failed to purge usage source: {}", dir.display()))
            }
        };
        // A failure here only delays it: the next read drops rows of files
        // that no longer exist.
        if let Err(error) = UsageIndex::open(root).and_then(|index| index.purge_source(source_id)) {
            eprintln!("usage index: purge of source {source_id} deferred: {error:#}");
        }
        Ok(removed)
    })
}

/// Catch up native ledger tails under the writer lock; membership itself is
/// a keyed query, never a clone of every historical thread ID.
pub(super) fn native_thread_index(root: &Path) -> anyhow::Result<UsageIndex> {
    with_usage_store_lock(root, |_, _| {
        let index=UsageIndex::open(root)?;
        index.sync_native(root)?;
        Ok(index)
    })
}

fn flush_pending_entries(
    state_path: &Path,
    ledger_dir: &Path,
    state: &mut UsageStateFile,
) -> anyhow::Result<()> {
    if state.pending_entries.is_empty() {
        return Ok(());
    }
    let mut months = Vec::new();
    for entry in &state.pending_entries {
        months.push(append_ledger_entry(ledger_dir, entry)?);
    }
    if let Some(root) = ledger_dir.parent() {
        index_appended(root, "", months);
    }
    state.pending_entries.clear();
    state.validate()?;
    write_usage_state(state_path, state)
}

/// Advance the index over lines just appended (caller holds the usage
/// lock). The line is already durable in the ledger, so a failure is not
/// the writer's: the next read catches the tail up from the checkpoint.
fn index_appended(root: &Path, source: &str, mut months: Vec<String>) {
    months.sort();
    months.dedup();
    let keys = months
        .into_iter()
        .map(|month| LedgerFileKey { source: source.to_string(), month })
        .collect::<Vec<_>>();
    if let Err(error) = UsageIndex::open(root).and_then(|index| index.sync_files(root, &keys)) {
        eprintln!("usage index: catch-up deferred to the next read: {error:#}");
    }
}

/// Append one entry; returns the month (`YYYY-MM`) file it went to.
fn append_ledger_entry(ledger_dir: &Path, entry: &UsageLedgerEntry) -> anyhow::Result<String> {
    entry.validate()?;
    fs::create_dir_all(ledger_dir).with_context(|| {
        format!(
            "Failed to create management v2 usage ledger: {}",
            ledger_dir.display()
        )
    })?;
    secure_directory(ledger_dir)?;
    let observed_at = parse_timestamp(entry.observed_at())?;
    let month = format!("{:04}-{:02}", observed_at.year(), observed_at.month());
    let path = ledger_dir.join(format!("{month}.jsonl"));
    let mut options = OpenOptions::new();
    options.write(true).append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("Failed to open usage ledger: {}", path.display()))?;
    secure_file(&path)?;
    let encoded = serde_json::to_vec(entry).context("Failed to serialize usage ledger entry")?;
    // Prefixing with a newline isolates a torn final write from the next retry.
    file.write_all(b"\n")?;
    file.write_all(&encoded)?;
    file.sync_data()
        .with_context(|| format!("Failed to sync usage ledger: {}", path.display()))?;
    Ok(month)
}

struct LedgerFile {
    path: PathBuf,
    file: File,
    length: u64,
}

/// Capture handles and committed byte boundaries under the writer lock after
/// pending recovery. Appends after this point are excluded; a purge/replacement
/// cannot substitute a different file midway through this report. No JSON is
/// decoded or copied while holding the lock.
fn capture_ledger_files(
    dirs: &[PathBuf],
    include_stem: impl Fn(&str) -> bool,
) -> anyhow::Result<Vec<LedgerFile>> {
    let mut files = Vec::new();
    for dir in dirs {
        for path in ledger_month_files(dir, &include_stem)? {
            let file = File::open(&path)
                .with_context(|| format!("Failed to open usage ledger: {}", path.display()))?;
            let length = file.metadata()?.len();
            files.push(LedgerFile { path, file, length });
        }
    }
    Ok(files)
}

/// Decode the snapshot outside the writer lock. Directory/file order retains
/// first-seen entry identity semantics, even for concurrent external purges.
fn read_captured_ledger(files: Vec<LedgerFile>) -> anyhow::Result<UsageLedger> {
    let mut ledger = UsageLedger::default();
    let mut seen = HashSet::new();
    for file in files {
        read_ledger_file(file, &mut seen, &mut ledger)?;
    }
    ledger.entries.sort_by(|left, right| {
        left.observed_at()
            .cmp(right.observed_at())
            .then_with(|| left.entry_id().cmp(right.entry_id()))
    });
    Ok(ledger)
}

fn ledger_month_files(
    ledger_dir: &Path,
    include_stem: &impl Fn(&str) -> bool,
) -> anyhow::Result<Vec<PathBuf>> {
    let mut paths = match fs::read_dir(ledger_dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|value| value == "jsonl"))
            .filter(|path| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(include_stem)
            })
            .collect::<Vec<_>>(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to read usage ledger: {}", ledger_dir.display()));
        }
    };
    paths.sort();
    Ok(paths)
}

fn read_ledger_file(
    captured: LedgerFile,
    seen: &mut HashSet<String>,
    ledger: &mut UsageLedger,
) -> anyhow::Result<()> {
    let LedgerFile { path, file, length } = captured;
    #[cfg(test)]
    if USAGE_LOCK_DEPTH.with(|depth| depth.get() > 0) {
        LOCKED_PARSE_BYTES.with(|bytes| bytes.set(bytes.get() + length));
    }
    for (index, line) in BufReader::new(file.take(length)).lines().enumerate() {
        let line_number = index + 1;
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                ledger.warnings.push(format!(
                    "{}:{line_number}: failed to read usage ledger line: {error}",
                    path.display()
                ));
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let entry = match serde_json::from_str::<UsageLedgerEntry>(&line) {
            Ok(entry) => entry,
            Err(error) => {
                ledger.warnings.push(format!(
                    "{}:{line_number}: ignored invalid usage ledger JSON: {error}",
                    path.display()
                ));
                continue;
            }
        };
        if let Err(error) = entry.validate() {
            ledger.warnings.push(format!(
                "{}:{line_number}: ignored invalid usage ledger entry: {error:#}",
                path.display()
            ));
            continue;
        }
        if seen.insert(entry.entry_id().to_string()) {
            ledger.entries.push(entry);
        }
    }
    Ok(())
}

fn with_usage_store_lock<T>(
    root: &Path,
    action: impl FnOnce(&Path, &Path) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    use crate::platform::lock_accounting::{Attempt, Kind, Lock};
    let mut attempt = Attempt::begin(Lock::Usage, Kind::Blocking);
    let _process_guard = USAGE_STORE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| anyhow::anyhow!("management v2 usage lock was poisoned"))?;
    fs::create_dir_all(root)?;
    secure_directory(root)?;
    let lock_file = open_private_lock(&root.join(USAGE_LOCK_FILE))?;
    lock_file.lock_exclusive()?;
    attempt.acquired();
    #[cfg(test)]
    let _depth = TestLockDepth::enter();
    let result = action(&root.join(USAGE_STATE_FILE), &root.join(USAGE_LEDGER_DIR));
    let unlock = lock_file.unlock();
    if result.is_ok() {
        unlock?;
    }
    drop(lock_file);
    drop(_process_guard);
    attempt.released();
    result
}

#[cfg(test)]
thread_local! {
    static USAGE_LOCK_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static LOCKED_PARSE_BYTES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}
#[cfg(test)]
struct TestLockDepth;
#[cfg(test)]
impl TestLockDepth {
    fn enter() -> Self { USAGE_LOCK_DEPTH.with(|n|n.set(n.get()+1)); Self }
}
#[cfg(test)]
impl Drop for TestLockDepth {
    fn drop(&mut self) { USAGE_LOCK_DEPTH.with(|n|n.set(n.get()-1)); }
}

#[cfg(test)]
#[path = "store_scale_tests.rs"]
mod scale_tests;

pub(super) fn load_usage_state(path: &Path) -> anyhow::Result<UsageStateFile> {
    let state = match fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<UsageStateFile>(&bytes)
            .with_context(|| format!("Failed to parse management v2 usage: {}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => UsageStateFile::default(),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("Failed to read management v2 usage: {}", path.display())
            });
        }
    };
    state.validate()?;
    Ok(state)
}

pub(super) fn write_usage_state(path: &Path, state: &UsageStateFile) -> anyhow::Result<()> {
    write_private_pretty_json_atomic(path, state, "management v2 session usage")
}

fn open_private_lock(path: &Path) -> anyhow::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    let file = options.open(path)?;
    secure_file(path)?;
    Ok(file)
}

#[cfg(unix)]
pub(super) fn secure_directory(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn secure_directory(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(unix)]
pub(super) fn secure_file(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn secure_file(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
pub(super) fn state_path(root: &Path) -> std::path::PathBuf {
    root.join(USAGE_STATE_FILE)
}

#[cfg(test)]
mod cleanup_tests {
    use super::*;
    #[test]
    fn deleted_projection_keeps_other_sessions_and_ledger() {
        let root = std::env::temp_dir().join(format!("usage-cleanup-{}", uuid::Uuid::new_v4()));
        with_usage_store_lock(&root, |path, ledger| {
            let mut state = UsageStateFile::default();
            state.sessions.insert("gone".into(), Default::default());
            state.sessions.insert("keep".into(), Default::default());
            fs::create_dir_all(ledger)?;
            fs::write(ledger.join("2026-10.jsonl"), b"ledger fixture")?;
            write_usage_state(path, &state)
        }).unwrap();
        forget_deleted_session_at(&root, "gone").unwrap();
        let loaded = load_usage_state(&state_path(&root)).unwrap();
        assert!(!loaded.sessions.contains_key("gone"));
        assert!(loaded.sessions.contains_key("keep"));
        assert!(loaded.deleted_sessions.contains("gone"));
        // Deleting a session that never received usage must fence its first late event too.
        forget_deleted_session_at(&root, "not-yet-observed").unwrap();
        assert!(load_usage_state(&state_path(&root)).unwrap().deleted_sessions.contains("not-yet-observed"));
        assert_eq!(fs::read(root.join(USAGE_LEDGER_DIR).join("2026-10.jsonl")).unwrap(), b"ledger fixture");
        fs::remove_dir_all(root).unwrap();
    }
}

/// The usage store lock as a value, for a reader that must release it
/// partway (after pinning its index snapshot). Dropping it unlocks.
pub(super) struct UsageStoreGuard {
    attempt: crate::platform::lock_accounting::Attempt,
    file: Option<File>,
    process: Option<std::sync::MutexGuard<'static, ()>>,
    #[cfg(test)]
    depth: Option<TestLockDepth>,
}

impl UsageStoreGuard {
    fn lock(root: &Path) -> anyhow::Result<Self> {
        use crate::platform::lock_accounting::{Attempt, Kind, Lock};
        let mut attempt = Attempt::begin(Lock::Usage, Kind::Blocking);
        let process = USAGE_STORE_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .map_err(|_| anyhow::anyhow!("management v2 usage lock was poisoned"))?;
        fs::create_dir_all(root)?;
        secure_directory(root)?;
        let file = open_private_lock(&root.join(USAGE_LOCK_FILE))?;
        file.lock_exclusive()?;
        attempt.acquired();
        Ok(Self {
            attempt,
            file: Some(file),
            process: Some(process),
            #[cfg(test)]
            depth: Some(TestLockDepth::enter()),
        })
    }
}

impl Drop for UsageStoreGuard {
    fn drop(&mut self) {
        #[cfg(test)]
        self.depth.take();
        if let Some(file) = self.file.take() {
            let _ = file.unlock();
        }
        self.process.take();
        self.attempt.released();
    }
}
