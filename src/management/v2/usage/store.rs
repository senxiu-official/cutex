use std::collections::HashSet;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
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

const USAGE_STATE_FILE: &str = "session-usage-state.json";
const USAGE_LOCK_FILE: &str = "session-usage-state.lock";
const USAGE_LEDGER_DIR: &str = "usage-ledger";
/// Rows imported from extra usage sources (`external.rs`): one directory per
/// source id, same `YYYY-MM.jsonl` layout. Removing a source's directory
/// purges exactly its rows.
pub(super) const USAGE_EXTERNAL_DIR: &str = "usage-ledger-external";

static USAGE_STORE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Debug, Default)]
pub(super) struct UsageMutation {
    pub changed: bool,
    pub entries: Vec<UsageLedgerEntry>,
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

pub(super) fn load_usage_ledger_at(root: &Path) -> anyhow::Result<UsageLedger> {
    with_usage_store_lock(root, |state_path, ledger_dir| {
        let mut state = load_usage_state(state_path)?;
        flush_pending_entries(state_path, ledger_dir, &mut state)?;
        read_usage_ledger(ledger_dir)
    })
}

pub(super) fn load_usage_data_at(root: &Path) -> anyhow::Result<(UsageStateSnapshot, UsageLedger)> {
    with_usage_store_lock(root, |state_path, ledger_dir| {
        let mut state = load_usage_state(state_path)?;
        flush_pending_entries(state_path, ledger_dir, &mut state)?;
        Ok((state.snapshot(), read_usage_ledger(ledger_dir)?))
    })
}

/// Like `load_usage_ledger_at`, but only reads `YYYY-MM.jsonl` files whose
/// month is within `[first_month, last_month]` (strings `YYYY-MM`). Files
/// with any other name are skipped. Rows of extra usage sources are merged
/// in (after cutex's own rows), except those of the source ids in `hidden`.
pub(super) fn load_usage_ledger_months_at(
    root: &Path,
    first_month: &str,
    last_month: &str,
    hidden: &HashSet<String>,
) -> anyhow::Result<UsageLedger> {
    with_usage_store_lock(root, |state_path, ledger_dir| {
        let mut state = load_usage_state(state_path)?;
        flush_pending_entries(state_path, ledger_dir, &mut state)?;
        let mut dirs = vec![ledger_dir.to_path_buf()];
        dirs.extend(external_source_dirs(root, hidden)?);
        read_usage_ledger_dirs(&dirs, |stem| {
            stem.len() == 7 && stem >= first_month && stem <= last_month
        })
    })
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
        for entry in entries {
            append_ledger_entry(&dir, entry)?;
        }
        Ok(())
    })
}

/// Delete every row (and the scan state) of one extra usage source.
pub(super) fn purge_external_source(root: &Path, source_id: &str) -> anyhow::Result<bool> {
    with_usage_store_lock(root, |_, _| {
        let dir = root.join(USAGE_EXTERNAL_DIR).join(source_id);
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error)
                .with_context(|| format!("Failed to purge usage source: {}", dir.display())),
        }
    })
}

/// Thread ids present in cutex's own ledger (all months). Cached per file
/// by (length, mtime), so a periodic caller re-reads only the month that
/// grew. Extra sources skip these (a cutex-run Codex thread is never
/// counted twice).
pub(super) fn ledger_thread_ids(root: &Path) -> anyhow::Result<HashSet<String>> {
    type Cache = std::collections::HashMap<PathBuf, (u64, Option<std::time::SystemTime>, HashSet<String>)>;
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    #[derive(serde::Deserialize)]
    struct ThreadOnly {
        #[serde(rename = "threadId")]
        thread_id: Option<String>,
    }
    let dir = root.join(USAGE_LEDGER_DIR);
    let paths = match fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|value| value == "jsonl"))
            .collect::<Vec<_>>(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    let mut cache = CACHE
        .get_or_init(|| Mutex::new(Cache::new()))
        .lock()
        .map_err(|_| anyhow::anyhow!("usage thread id cache was poisoned"))?;
    cache.retain(|path, _| paths.contains(path));
    let mut out = HashSet::new();
    for path in paths {
        let meta = fs::metadata(&path)?;
        let signature = (meta.len(), meta.modified().ok());
        let fresh = cache
            .get(&path)
            .is_some_and(|(len, mtime, _)| (*len, *mtime) == signature);
        if !fresh {
            let mut ids = HashSet::new();
            let file = File::open(&path)?;
            for line in BufReader::new(file).lines() {
                let Ok(line) = line else { continue };
                if !line.contains("\"threadId\"") {
                    continue;
                }
                if let Ok(ThreadOnly { thread_id: Some(id) }) = serde_json::from_str(&line) {
                    ids.insert(id);
                }
            }
            cache.insert(path.clone(), (signature.0, signature.1, ids));
        }
        if let Some((_, _, ids)) = cache.get(&path) {
            out.extend(ids.iter().cloned());
        }
    }
    Ok(out)
}

fn flush_pending_entries(
    state_path: &Path,
    ledger_dir: &Path,
    state: &mut UsageStateFile,
) -> anyhow::Result<()> {
    if state.pending_entries.is_empty() {
        return Ok(());
    }
    for entry in &state.pending_entries {
        append_ledger_entry(ledger_dir, entry)?;
    }
    state.pending_entries.clear();
    state.validate()?;
    write_usage_state(state_path, state)
}

fn append_ledger_entry(ledger_dir: &Path, entry: &UsageLedgerEntry) -> anyhow::Result<()> {
    entry.validate()?;
    fs::create_dir_all(ledger_dir).with_context(|| {
        format!(
            "Failed to create management v2 usage ledger: {}",
            ledger_dir.display()
        )
    })?;
    secure_directory(ledger_dir)?;
    let observed_at = parse_timestamp(entry.observed_at())?;
    let path = ledger_dir.join(format!(
        "{:04}-{:02}.jsonl",
        observed_at.year(),
        observed_at.month()
    ));
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
    Ok(())
}

fn read_usage_ledger(ledger_dir: &Path) -> anyhow::Result<UsageLedger> {
    read_usage_ledger_filtered(ledger_dir, |_| true)
}

fn read_usage_ledger_filtered(
    ledger_dir: &Path,
    include_stem: impl Fn(&str) -> bool,
) -> anyhow::Result<UsageLedger> {
    read_usage_ledger_dirs(&[ledger_dir.to_path_buf()], include_stem)
}

/// Read every month file of `dirs` in order; an entry id seen in an earlier
/// directory (or file) wins.
fn read_usage_ledger_dirs(
    dirs: &[PathBuf],
    include_stem: impl Fn(&str) -> bool,
) -> anyhow::Result<UsageLedger> {
    let mut ledger = UsageLedger::default();
    let mut seen = HashSet::new();
    for dir in dirs {
        for path in ledger_month_files(dir, &include_stem)? {
            read_ledger_file(&path, &mut seen, &mut ledger)?;
        }
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
    path: &Path,
    seen: &mut HashSet<String>,
    ledger: &mut UsageLedger,
) -> anyhow::Result<()> {
    let file = File::open(path)
        .with_context(|| format!("Failed to open usage ledger: {}", path.display()))?;
    for (index, line) in BufReader::new(file).lines().enumerate() {
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
    let _process_guard = USAGE_STORE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| anyhow::anyhow!("management v2 usage lock was poisoned"))?;
    fs::create_dir_all(root)?;
    secure_directory(root)?;
    let lock_file = open_private_lock(&root.join(USAGE_LOCK_FILE))?;
    lock_file.lock_exclusive()?;
    let result = action(&root.join(USAGE_STATE_FILE), &root.join(USAGE_LEDGER_DIR));
    let unlock = lock_file.unlock();
    if result.is_ok() {
        unlock?;
    }
    result
}

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
