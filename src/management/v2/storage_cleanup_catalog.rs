//! Design 043 A3/A4 discovery only. No removal, rename, config reads or repair writes.
//! The cleanup engine must still fence paths/identities and check the entire tree's newest mtime.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, Metadata};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{ensure, Context, Result};
use rusqlite::{Connection, OpenFlags};

const DAY: Duration = Duration::from_secs(86_400);
const MAX_ENTRIES: usize = 100_000;
pub(super) struct Discovery {
    pub(super) items: Vec<Candidate>,
    pub(super) partial_reason: Option<&'static str>,
}
struct DiscoveryBudget {
    deadline: Option<Instant>,
    partial_reason: Option<&'static str>,
}
impl DiscoveryBudget {
    fn proceed(&mut self) -> bool {
        if self.deadline.is_some_and(|end| Instant::now() >= end) {
            self.partial_reason.get_or_insert("time_budget");
        }
        self.partial_reason.is_none()
    }
}
const FORK_RECEIPTS: &str = "runtime/claude/fork-copy-leftovers";
const FORK_MANUAL_REASON: &str = "Claude cross-account copy inspection receipt retained after interruption, failure or receipt cleanup failure. The receipt records the exact staged transcript, target UUID directory and source backup. External account history is outside the cleanup root: inspect those paths manually; this preview cannot authorize deleting them or their receipt. No automatic deletion.";

fn fork_receipt(path: &Path) -> bool {
    path.parent() == Some(Path::new(FORK_RECEIPTS))
        && path
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| uuid::Uuid::parse_str(s).is_ok_and(|id| id.to_string() == s))
        && path.extension().is_some_and(|s| s == "json")
}

#[cfg(test)]
thread_local! {
    static LISTED_PATHS: std::cell::RefCell<Option<Vec<PathBuf>>> = const { std::cell::RefCell::new(None) };
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Candidate {
    pub(super) relative: PathBuf,
    pub(super) eligible: bool,
    pub(super) code: String,
    pub(super) reason: String,
}

fn decision(relative: &Path, code: &str, reason: &str) -> Candidate {
    Candidate {
        relative: relative.into(),
        eligible: code == "eligible",
        code: code.into(),
        reason: reason.into(),
    }
}

fn linked(meta: &Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    false
}

fn relative_ok(path: &Path) -> bool {
    !path.as_os_str().is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)))
}

/// Never canonicalize through a junction, including ancestors of the supplied home.
fn check_root(root: &Path) -> Result<()> {
    ensure!(root.is_absolute(), "storage cleanup root must be absolute");
    ensure!(
        !root
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir)),
        "storage cleanup root must be normalized"
    );
    for part in root.ancestors() {
        let m = fs::symlink_metadata(part)?;
        ensure!(
            m.is_dir() && !linked(&m),
            "storage cleanup root contains a link or non-directory"
        );
    }
    Ok(())
}

fn metadata(root: &Path, relative: &Path) -> Result<Metadata> {
    ensure!(
        relative_ok(relative),
        "candidate must be a normalized relative path"
    );
    let mut path = root.to_path_buf();
    let mut parts = relative.components().peekable();
    while let Some(part) = parts.next() {
        path.push(part.as_os_str());
        let m = fs::symlink_metadata(&path)?;
        ensure!(!linked(&m), "path contains a symlink or reparse point");
        if parts.peek().is_some() {
            ensure!(m.is_dir(), "path parent is not a directory");
        } else {
            return Ok(m);
        }
    }
    unreachable!()
}

fn key(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        text.to_ascii_lowercase()
    } else {
        text
    }
}

fn protected(root: &Path, candidate: &Path, paths: &[PathBuf]) -> bool {
    let c = key(&root.join(candidate));
    paths.iter().any(|p| {
        let p = key(&if p.is_absolute() {
            p.clone()
        } else {
            root.join(p)
        });
        c == p || c.starts_with(&(p.clone() + "/")) || p.starts_with(&(c.clone() + "/"))
    })
}

fn old_enough(m: &Metadata, now: SystemTime, days: u32) -> bool {
    m.modified()
        .ok()
        .and_then(|t| now.duration_since(t).ok())
        .is_some_and(|age| age > DAY.saturating_mul(days))
}

fn children(root: &Path, relative: &Path) -> Result<Vec<PathBuf>> {
    children_bounded(
        root,
        relative,
        &mut DiscoveryBudget {
            deadline: None,
            partial_reason: None,
        },
    )
}
fn children_bounded(
    root: &Path,
    relative: &Path,
    budget: &mut DiscoveryBudget,
) -> Result<Vec<PathBuf>> {
    if !budget.proceed() {
        return Ok(vec![]);
    }
    #[cfg(test)]
    LISTED_PATHS.with(|paths| {
        if let Some(paths) = paths.borrow_mut().as_mut() {
            paths.push(relative.into());
        }
    });
    if !relative.as_os_str().is_empty() {
        match metadata(root, relative) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Ok(vec![]),
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(vec![])
            }
            Err(e) => return Err(e),
        }
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(root.join(relative))? {
        if !budget.proceed() {
            break;
        }
        let entry = entry?;
        if out.len() >= MAX_ENTRIES && budget.deadline.is_some() {
            budget.partial_reason = Some("entry_limit");
            break;
        }
        ensure!(
            out.len() < MAX_ENTRIES,
            "storage cleanup directory exceeds discovery budget"
        );
        out.push(relative.join(entry.file_name()));
    }
    out.sort();
    Ok(out)
}

fn digits(s: &str) -> bool {
    !s.is_empty() && s.len() <= 20 && s.bytes().all(|c| c.is_ascii_digit())
}

// Historical backup timestamps: compact UTC dates/times or epoch values, with a numeric collision suffix.
fn stamp(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 40
        && s.bytes().any(|b| b.is_ascii_digit())
        && s.bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'-' | b'T' | b'Z'))
        && !s.starts_with('-')
        && !s.ends_with('-')
        && !s.contains("--")
}

fn native_name(s: &str) -> Option<&str> {
    let main = s
        .strip_suffix("-wal")
        .or_else(|| s.strip_suffix("-shm"))
        .unwrap_or(s);
    let (family, version) = main.strip_suffix(".sqlite")?.split_once('_')?;
    (matches!(family, "state" | "logs") && digits(version)).then_some(main)
}

fn native_group(s: &str) -> bool {
    s.strip_prefix("sqlite-")
        .and_then(|s| s.split_once('-'))
        .is_some_and(|(at, seq)| digits(at) && digits(seq))
}

fn dated_group(s: &str, prefix: &str) -> bool {
    s.strip_prefix(prefix).is_some_and(|date| {
        date.get(..8)
            .is_some_and(|day| chrono::NaiveDate::parse_from_str(day, "%Y%m%d").is_ok())
            && (date.len() == 8 || matches!(date.as_bytes().get(8), Some(b'-' | b'T')))
            && stamp(date)
    })
}

// The P1 classification is deliberately broader than this deletion allowlist. Unproven historical
// aggregates remain visible but ineligible; new names must be added with their writer/counterpart.
fn backup_name(path: &Path) -> bool {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    match path.parent().and_then(|p| p.to_str()) {
        Some("") => [
            "cutex-sessions.json.bak-",
            "cutex-sessions.json.pre-sqlite-",
            "cutex-sessions.json.sentinel-leaked-",
            ".pre-sqlite-",
            ".sentinel-leaked-",
            "config.before-",
        ]
        .iter()
        .any(|prefix| name.strip_prefix(prefix).is_some_and(stamp)),
        Some("codex-home") => {
            dated_group(name, "logs-db-backup-")
                || dated_group(name, "state-db-backup-")
                || name == "sqlite-migration-backup"
                || dated_group(name, "sqlite-migration-backup-")
                || [".bak", ".bak-", ".bak."].iter().any(|marker| {
                    name.split_once(marker).is_some_and(|(base, suffix)| {
                        native_name(base).is_some()
                            && (*marker == ".bak" && suffix.is_empty() || stamp(suffix))
                    })
                })
        }
        _ => path.parent() == Some(Path::new("codex-home/db-backups")) && native_group(name),
    }
}

fn healthy_db(root: &Path, relative: &Path, sessions: bool) -> Result<()> {
    let m = metadata(root, relative)?;
    ensure!(
        m.is_file() && m.len() > 0,
        "current database is absent or empty"
    );
    // SQLite may open sidecars itself. Reject linked sidecars before handing the path to SQLite.
    for suffix in ["-wal", "-shm", "-journal"] {
        let side = PathBuf::from(format!(
            "{}{suffix}",
            relative.to_str().context("invalid database path")?
        ));
        match metadata(root, &side) {
            Ok(m) => ensure!(
                m.is_file(),
                "current database sidecar is not a regular file"
            ),
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => {}
            Err(e) => {
                return Err(e).context(
                    "current database sidecar cannot be verified without following a link",
                )
            }
        }
    }
    let db = Connection::open_with_flags(
        root.join(relative),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    db.busy_timeout(Duration::ZERO)?;
    let start = Instant::now();
    db.progress_handler(
        10_000,
        Some(move || start.elapsed() > Duration::from_secs(2)),
    )?;
    let check: String = db.query_row("PRAGMA quick_check(1)", [], |r| r.get(0))?;
    ensure!(check == "ok", "current database quick_check failed");
    let tables: i64 = db.query_row(
        "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        [],
        |r| r.get(0),
    )?;
    ensure!(tables > 0, "current database has no application schema");
    if sessions {
        let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let schema: String = db.query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )?;
        db.prepare(
            "SELECT cutex_session_id, archive_state, rev, record_json FROM sessions LIMIT 0",
        )?;
        db.prepare("SELECT map, key, rev, json FROM receipts LIMIT 0")?;
        ensure!(
            version == 1 && schema == "1",
            "current session database schema is not recognized"
        );
    }
    Ok(())
}

fn backup_health(
    root: &Path,
    relative: &Path,
    m: &Metadata,
    cache: &mut BTreeMap<PathBuf, bool>,
) -> Result<()> {
    for (path, sessions) in backup_originals(root, relative, m)? {
        let ok = *cache
            .entry(path.clone())
            .or_insert_with(|| healthy_db(root, &path, sessions).is_ok());
        ensure!(ok, "current counterpart database is missing, busy, corrupt or could not pass the bounded read-only health check");
    }
    Ok(())
}

fn backup_originals(
    root: &Path,
    relative: &Path,
    m: &Metadata,
) -> Result<BTreeSet<(PathBuf, bool)>> {
    let name = relative
        .file_name()
        .and_then(|s| s.to_str())
        .context("non-UTF8 backup name")?;
    ensure!(
        !name.starts_with("config.before-"),
        "config health cannot be established without a trusted config-owner check"
    );
    let mut originals = BTreeSet::new();
    if relative.parent() == Some(Path::new("")) {
        ensure!(
            m.is_file(),
            "session migration references must be regular files"
        );
        originals.insert((PathBuf::from("cutex-sessions.sqlite3"), true));
    } else if m.is_dir() {
        // Native recovery's db-backups/sqlite-<seconds>-<sequence> writes only the database and
        // its -wal/-shm siblings. Historical groups get the same strict leaf validation.
        for leaf in children(root, relative)? {
            let child = metadata(root, &leaf)?;
            ensure!(
                child.is_file(),
                "backup group contains a directory or unsupported member"
            );
            let original = native_name(leaf.file_name().and_then(|s| s.to_str()).unwrap_or(""))
                .context(
                    "backup group contains a name without a known current database counterpart",
                )?;
            originals.insert((PathBuf::from("codex-home").join(original), false));
        }
        ensure!(
            !originals.is_empty(),
            "backup group contains no known database"
        );
    } else {
        ensure!(m.is_file(), "unsupported backup type");
        let base = name.split(".bak").next().context("unknown backup name")?;
        let original = native_name(base).context("unknown database backup name")?;
        originals.insert((PathBuf::from("codex-home").join(original), false));
    }
    Ok(originals)
}

fn release_number(value: &str) -> Option<u64> {
    let s = value.strip_prefix('r')?;
    if !digits(s) {
        return None;
    }
    s.parse().ok()
}

fn release_log(path: &Path) -> Option<u64> {
    if path.parent()? != Path::new("logs") {
        return None;
    }
    let s = path.file_name()?.to_str()?;
    let rest = s
        .strip_prefix("agent-bus-r")
        .or_else(|| s.strip_prefix("management-r"))?;
    let (version, rotate) = rest.split_once(".log")?;
    if !digits(version) {
        return None;
    }
    let rotation = rotate.strip_suffix(".gz").unwrap_or(rotate);
    if !rotation.is_empty()
        && !rotation
            .strip_prefix('.')
            .is_some_and(|s| digits(s) && s.parse::<u32>().is_ok_and(|n| (1..=32).contains(&n)))
    {
        return None;
    }
    version.parse().ok()
}

/// Release logs use the independent 24-hour floor, not the normal-log selected age.
pub(super) fn is_release_log(relative: &Path) -> bool {
    release_log(relative).is_some()
}

fn reserved(path: &Path) -> bool {
    path.components().any(|part| {
        let n = part.as_os_str().to_string_lossy().to_ascii_lowercase();
        n.contains("audit")
            || n.contains("cleanup")
            || n.ends_with(".sqlite")
            || n.ends_with(".sqlite3")
            || n.ends_with("-wal")
            || n.ends_with("-shm")
            || n.strip_suffix("-journal").is_some_and(|base| {
                base.ends_with(".sqlite") || base.ends_with(".sqlite3") || base.ends_with(".db")
            })
    })
}

fn log_tree(
    root: &Path,
    relative: &Path,
    out: &mut Vec<PathBuf>,
    depth: usize,
    budget: &mut DiscoveryBudget,
) -> Result<()> {
    if !budget.proceed() {
        return Ok(());
    }
    if out.len() >= MAX_ENTRIES && budget.deadline.is_some() {
        budget.partial_reason = Some("entry_limit");
        return Ok(());
    }
    ensure!(
        depth < 32 && out.len() < MAX_ENTRIES,
        "log discovery exceeds traversal budget"
    );
    match metadata(root, relative) {
        Ok(m) if m.is_dir() => {}
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(())
        }
        _ => {
            out.push(relative.into());
            return Ok(());
        }
    }
    for path in children_bounded(root, relative, budget)? {
        if !budget.proceed() {
            break;
        }
        if out.len() >= MAX_ENTRIES && budget.deadline.is_some() {
            budget.partial_reason = Some("entry_limit");
            break;
        }
        ensure!(
            out.len() < MAX_ENTRIES,
            "log discovery exceeds traversal budget"
        );
        if reserved(&path) {
            continue;
        }
        match metadata(root, &path) {
            Ok(m) if m.is_dir() => log_tree(root, &path, out, depth + 1, budget)?,
            _ => out.push(path), // link/error becomes an explicit ineligible candidate below.
        }
    }
    Ok(())
}

/// Fixed writer/path allowlists only. `protected_paths` may be root-relative or absolute active paths.
/// Age here is preliminary; the engine must recheck every descendant and strong tree fingerprint.
pub(super) fn candidates(
    root: &Path,
    action: &str,
    older_than_days: u32,
    now: SystemTime,
    current_release: Option<&str>,
    protected_paths: &[PathBuf],
) -> Result<Vec<Candidate>> {
    Ok(candidates_with_deadline(
        root,
        action,
        older_than_days,
        now,
        current_release,
        protected_paths,
        None,
    )?
    .items)
}
pub(super) fn candidates_with_deadline(
    root: &Path,
    action: &str,
    older_than_days: u32,
    now: SystemTime,
    current_release: Option<&str>,
    protected_paths: &[PathBuf],
    deadline: Option<Instant>,
) -> Result<Discovery> {
    let mut budget = DiscoveryBudget {
        deadline,
        partial_reason: None,
    };
    if !budget.proceed() {
        return Ok(Discovery {
            items: vec![],
            partial_reason: budget.partial_reason,
        });
    }
    check_root(root)?;
    ensure!(
        protected_paths.iter().all(|p| !p
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))),
        "protected paths must be normalized"
    );
    ensure!(
        matches!(action, "old-backups" | "old-logs"),
        "unknown cleanup catalog action"
    );
    let days = if action == "old-backups" {
        older_than_days.max(14)
    } else {
        ensure!(
            matches!(older_than_days, 0 | 7 | 30 | 90),
            "log age must be 7, 30 or 90 days"
        );
        if older_than_days == 0 {
            30
        } else {
            older_than_days
        }
    };
    let mut paths = Vec::new();
    if action == "old-backups" {
        paths.extend(
            children(root, Path::new(""))?
                .into_iter()
                .filter(|p| backup_name(p)),
        );
        paths.extend(
            children(root, Path::new("codex-home"))?
                .into_iter()
                .filter(|p| backup_name(p)),
        );
        paths.extend(
            children(root, Path::new("codex-home/db-backups"))?
                .into_iter()
                .filter(|p| backup_name(p)),
        );
        // Repair backups can be unique user history. No automatic eligibility without its owner.
        for path in children(root, Path::new("history-repair-backups"))? {
            paths.push(path);
        }
        paths.extend(
            children(root, Path::new(FORK_RECEIPTS))?
                .into_iter()
                .filter(|p| fork_receipt(p)),
        );
    } else {
        paths.extend(
            children_bounded(root, Path::new("logs"), &mut budget)?
                .into_iter()
                .filter(|p| release_log(p).is_some()),
        );
        for prefix in [
            "runtime/diagnostics",
            "runtime/app-server-journal",
            "runtime/management-v2/orphaned",
        ] {
            log_tree(root, Path::new(prefix), &mut paths, 0, &mut budget)?;
        }
    }
    paths.sort();
    paths.dedup();
    let current = current_release.and_then(release_number);
    let mut previous = BTreeSet::new();
    if let Some(current) = current {
        for path in &paths {
            if let Some(version) = release_log(path) {
                if version < current {
                    previous.insert(version);
                }
            }
        }
    }
    let kept: BTreeSet<_> = previous.into_iter().rev().take(2).collect();
    let mut health = BTreeMap::new();
    let mut out = Vec::new();
    let discovery_partial = budget.partial_reason.take();
    for path in paths {
        if !budget.proceed() {
            break;
        }
        let item = if protected(root, &path, protected_paths) {
            decision(
                &path,
                "protected",
                "An active runtime or protected path still uses this item",
            )
        } else {
            match metadata(root, &path) {
                Err(_) => decision(
                    &path,
                    "path_unavailable",
                    "The item or a parent is unavailable, linked or unreadable",
                ),
                Ok(m) => {
                    if fork_receipt(&path) {
                        decision(&path, "manual_required", FORK_MANUAL_REASON)
                    } else if reserved(&path) {
                        decision(&path, "protected", "Audit, cleanup and current database files are never cleanup candidates")
                    } else if let Some(version) = release_log(&path) {
                        if !m.is_file() {
                            decision(&path, "not_file", "Release log must be a regular file")
                        } else if discovery_partial.is_some() {
                            decision(
                                &path,
                                "preview_incomplete",
                                "Incomplete discovery cannot authorize release log cleanup",
                            )
                        } else if current.is_none() {
                            decision(
                                &path,
                                "current_release_unknown",
                                "Current release is unknown; preserve all release logs",
                            )
                        } else if version >= current.unwrap() || kept.contains(&version) {
                            decision(&path, "release_kept", "Keep the current release, newer releases and the two latest prior release groups")
                        } else if !old_enough(&m, now, 1) {
                            decision(
                                &path,
                                "recent_write",
                                "Log was written within the last 24 hours or its age is unknown",
                            )
                        } else {
                            decision(
                                &path,
                                "eligible",
                                "Old release group; no write in the last 24 hours",
                            )
                        }
                    } else if !old_enough(&m, now, days) {
                        decision(&path, "too_recent", "Item has not passed the selected age or its modification time is unknown")
                    } else if action == "old-logs" {
                        if m.is_file() {
                            decision(&path, "eligible", "Old file in an explicitly listed diagnostic/journal/orphan directory; no write in the last 24 hours")
                        } else {
                            decision(&path, "not_file", "Only regular log files may be selected")
                        }
                    } else if path.starts_with("history-repair-backups") {
                        decision(&path, "history_health_unverified", "Preserve repair history until its owner can prove an equivalent current transcript exists")
                    } else {
                        match backup_health(root, &path, &m, &mut health) {
                    Ok(()) => decision(&path, "eligible", "Known backup older than 14 days; current counterpart database passed read-only quick_check"),
                    Err(e) => decision(&path, "counterpart_unverified", &e.to_string()),
                }
                    }
                }
            }
        };
        out.push(item);
    }
    Ok(Discovery {
        items: out,
        partial_reason: budget.partial_reason.or(discovery_partial),
    })
}

const MAX_RECHECK_PATHS: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
enum MetadataFence {
    Missing,
    Present {
        directory: bool,
        len: u64,
        modified: SystemTime,
        created: Option<SystemTime>,
        file: Option<crate::platform::file_stamp::FileStamp>,
        #[cfg(unix)]
        identity: (u64, u64, i64, i64),
        #[cfg(windows)]
        identity: (u64, u64),
    },
}

fn metadata_fence(path: &Path) -> Result<MetadataFence> {
    let m = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(MetadataFence::Missing),
        Err(e) => return Err(e.into()),
    };
    ensure!(
        !linked(&m) && (m.is_file() || m.is_dir()),
        "fenced path is linked or unsupported"
    );
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        (m.dev(), m.ino(), m.ctime(), m.ctime_nsec())
    };
    #[cfg(windows)]
    let identity = {
        use std::os::windows::fs::OpenOptionsExt;
        let file = fs::OpenOptions::new()
            .access_mode(windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES)
            .share_mode(7) // READ | WRITE | DELETE: observation only, never a persistent directory pin.
            .custom_flags(
                windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS
                    | windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
            )
            .open(path)?;
        ensure!(
            !linked(&file.metadata()?),
            "opened fenced path is a reparse point"
        );
        crate::platform::private_fs::identity(&file)?.publication_key()
    };
    let file = if m.is_file() {
        Some(
            crate::platform::file_stamp::FileStamp::of(path)
                .context("fenced file changed while statting")?,
        )
    } else {
        None
    };
    Ok(MetadataFence::Present {
        directory: m.is_dir(),
        len: m.len(),
        modified: m.modified()?,
        created: m.created().ok(),
        file,
        identity,
    })
}

/// Prepared OUTSIDE session/lifecycle locks. The caller separately fences the session/protected-
/// activity revision and the preview's recursive fingerprint through the root rename. This ticket
/// only fences fixed catalog dependencies; it never authorizes deleting an unpreviewed tree.
#[derive(Clone, Debug)]
pub(super) struct PreparedCatalogRecheck {
    pub(super) candidate: Candidate,
    root: PathBuf,
    paths: BTreeMap<PathBuf, MetadataFence>,
}

impl PreparedCatalogRecheck {
    fn fence(&mut self, relative: &Path) -> Result<()> {
        ensure!(
            relative.as_os_str().is_empty() || relative_ok(relative),
            "invalid catalog fence path"
        );
        for path in relative.ancestors().collect::<Vec<_>>().into_iter().rev() {
            if !self.paths.contains_key(path) {
                ensure!(
                    self.paths.len() < MAX_RECHECK_PATHS,
                    "catalog recheck exceeds fixed-path budget"
                );
                self.paths
                    .insert(path.into(), metadata_fence(&self.root.join(path))?);
            }
        }
        Ok(())
    }
}

fn normalized_protection(paths: &[PathBuf]) -> Result<()> {
    ensure!(
        paths.iter().all(|p| !p
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))),
        "protected paths must be normalized"
    );
    Ok(())
}

fn selected_log(relative: &Path) -> bool {
    [
        "runtime/diagnostics",
        "runtime/app-server-journal",
        "runtime/management-v2/orphaned",
    ]
    .iter()
    .any(|prefix| relative.starts_with(prefix) && relative != Path::new(prefix))
}

/// Performs only the selected item's discovery/health work. No traversal of unrelated homes,
/// diagnostics or runtime journals. Release-group names are listed once from `logs/` only.
pub(super) fn prepare_recheck(
    root: &Path,
    action: &str,
    relative: &Path,
    older_than_days: u32,
    now: SystemTime,
    current_release: Option<&str>,
    protected_paths: &[PathBuf],
) -> Result<PreparedCatalogRecheck> {
    check_root(root)?;
    normalized_protection(protected_paths)?;
    ensure!(
        relative_ok(relative),
        "candidate must be a normalized relative path"
    );
    ensure!(
        matches!(action, "old-backups" | "old-logs"),
        "unknown cleanup catalog action"
    );
    ensure!(
        action != "old-logs" || matches!(older_than_days, 0 | 7 | 30 | 90),
        "log age must be 7, 30 or 90 days"
    );
    let mut ticket = PreparedCatalogRecheck {
        candidate: decision(
            relative,
            "not_allowlisted",
            "Path is not in this action's fixed allowlist",
        ),
        root: root.into(),
        paths: BTreeMap::new(),
    };
    let history_group = relative.parent() == Some(Path::new("history-repair-backups"));
    let allowed = if action == "old-backups" {
        backup_name(relative) || history_group || fork_receipt(relative)
    } else {
        is_release_log(relative) || selected_log(relative)
    };
    if !allowed {
        return Ok(ticket);
    }
    if protected(root, relative, protected_paths) || reserved(relative) {
        ticket.candidate = decision(
            relative,
            "protected",
            "An active runtime, audit/cleanup file or protected path still uses this item",
        );
        return Ok(ticket);
    }
    if ticket.fence(relative).is_err() {
        ticket.candidate = decision(
            relative,
            "path_unavailable",
            "Item or parent is unavailable, linked or unreadable",
        );
        return Ok(ticket);
    }
    let m = match metadata(root, relative) {
        Ok(m) => m,
        Err(_) => {
            ticket.candidate = decision(
                relative,
                "path_unavailable",
                "Item or parent is unavailable, linked or unreadable",
            );
            return Ok(ticket);
        }
    };
    ticket.candidate = if fork_receipt(relative) {
        decision(relative, "manual_required", FORK_MANUAL_REASON)
    } else if let Some(version) = release_log(relative) {
        if !m.is_file() {
            decision(relative, "not_file", "Release log must be a regular file")
        } else if let Some(current) = current_release.and_then(release_number) {
            ticket.fence(Path::new("logs"))?;
            let kept: BTreeSet<_> = children(root, Path::new("logs"))?
                .iter()
                .filter_map(|p| release_log(p))
                .filter(|v| *v < current)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .rev()
                .take(2)
                .collect();
            if version >= current || kept.contains(&version) {
                decision(
                    relative,
                    "release_kept",
                    "Keep current/newer releases and the two latest prior release groups",
                )
            } else if !old_enough(&m, now, 1) {
                decision(
                    relative,
                    "recent_write",
                    "Log was written within the last 24 hours or its age is unknown",
                )
            } else {
                decision(
                    relative,
                    "eligible",
                    "Old release group; no write in the last 24 hours",
                )
            }
        } else {
            decision(
                relative,
                "current_release_unknown",
                "Current release is unknown; preserve release logs",
            )
        }
    } else {
        let days = if action == "old-backups" {
            older_than_days.max(14)
        } else if older_than_days == 0 {
            30
        } else {
            older_than_days
        };
        if !old_enough(&m, now, days) {
            decision(
                relative,
                "too_recent",
                "Item has not passed the selected age or its modification time is unknown",
            )
        } else if action == "old-logs" {
            if m.is_file() {
                decision(
                    relative,
                    "eligible",
                    "Old regular file in a fixed diagnostic/journal/orphan directory",
                )
            } else {
                decision(
                    relative,
                    "not_file",
                    "Only regular log files may be selected",
                )
            }
        } else if history_group {
            decision(relative, "history_health_unverified", "Preserve unique repair history until its owner proves an equivalent transcript exists")
        } else {
            let verified = (|| -> Result<()> {
                if m.is_dir() {
                    for child in children(root, relative)? {
                        ticket.fence(&child)?;
                    }
                }
                let originals = backup_originals(root, relative, &m)?;
                for (path, _) in &originals {
                    ticket.fence(path)?;
                    for suffix in ["-wal", "-shm", "-journal"] {
                        ticket.fence(&PathBuf::from(format!(
                            "{}{suffix}",
                            path.to_str().context("invalid database path")?
                        )))?;
                    }
                }
                for (path, sessions) in originals {
                    healthy_db(root, &path, sessions)?;
                }
                Ok(())
            })();
            match verified {
                Ok(()) => decision(
                    relative,
                    "eligible",
                    "Known old backup; current counterparts passed read-only health checks",
                ),
                Err(e) => decision(relative, "counterpart_unverified", &e.to_string()),
            }
        }
    };
    // The health query/listing must refer to exactly the metadata captured before it started.
    if ticket.candidate.eligible
        && !recheck_prepared(root, &ticket, Instant::now() + Duration::from_secs(2))?
    {
        ticket.candidate = decision(
            relative,
            "changed_during_prepare",
            "Selected item or its catalog dependencies changed during preparation",
        );
    }
    Ok(ticket)
}

/// Lock-safe validation: fixed metadata only, no directory enumeration, SQLite, content reads or
/// recursive fingerprints. The deadline is checked before AND after each fixed-path observation;
/// an OS call cannot be interrupted, so an over-budget observation always rejects the ticket.
pub(super) fn recheck_prepared(
    root: &Path,
    ticket: &PreparedCatalogRecheck,
    deadline: Instant,
) -> Result<bool> {
    if root != ticket.root.as_path()
        || !ticket.candidate.eligible
        || ticket.paths.len() > MAX_RECHECK_PATHS
    {
        return Ok(false);
    }
    for (relative, expected) in &ticket.paths {
        if Instant::now() >= deadline {
            return Ok(false);
        }
        let actual = metadata_fence(&root.join(relative));
        if Instant::now() >= deadline || actual.as_ref().ok() != Some(expected) {
            return Ok(false);
        }
    }
    Ok(Instant::now() < deadline)
}

/// Compatibility entrypoint. Call outside locks; the engine should retain the prepared ticket.
pub(super) fn recheck(
    root: &Path,
    action: &str,
    candidate_relative: &Path,
    older_than_days: u32,
    now: SystemTime,
    current_release: Option<&str>,
    protected_paths: &[PathBuf],
) -> Result<Candidate> {
    Ok(prepare_recheck(
        root,
        action,
        candidate_relative,
        older_than_days,
        now,
        current_release,
        protected_paths,
    )?
    .candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fork_copy_receipts_are_visible_but_never_external_delete_authority() {
        let f = Fixture::new();
        let path = format!("{FORK_RECEIPTS}/550e8400-e29b-41d4-a716-446655440000.json");
        f.file(&path, 40);
        let found = candidates(&f.root, "old-backups", 14, f.now, None, &[]).unwrap();
        let candidate = found
            .iter()
            .find(|item| item.relative == Path::new(&path))
            .unwrap();
        assert!(!candidate.eligible);
        assert_eq!(candidate.code, "manual_required");
        assert!(candidate
            .reason
            .contains("External account history is outside the cleanup root"));
        let ticket = prepare_recheck(
            &f.root,
            "old-backups",
            Path::new(&path),
            14,
            f.now,
            None,
            &[],
        )
        .unwrap();
        assert!(!ticket.candidate.eligible);
        assert_eq!(ticket.candidate.code, "manual_required");
        assert!(f.root.join(&path).is_file());
    }

    struct Fixture {
        root: PathBuf,
        now: SystemTime,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir()
                .join(format!("cutex-cleanup-catalog-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            Self {
                root,
                now: SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000),
            }
        }
        fn age(&self, relative: &str, days: u32) {
            let path = self.root.join(relative);
            #[cfg(windows)]
            let file = {
                use std::os::windows::fs::OpenOptionsExt;
                fs::OpenOptions::new()
                    .access_mode(0x100)
                    .custom_flags(0x02000000)
                    .open(path)
                    .unwrap()
            };
            #[cfg(not(windows))]
            let file = fs::File::open(path).unwrap();
            file.set_times(fs::FileTimes::new().set_modified(self.now - DAY.saturating_mul(days)))
                .unwrap();
        }
        fn file(&self, relative: &str, days: u32) {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"synthetic fixture").unwrap();
            self.age(relative, days);
        }
        fn db(&self, relative: &str, sessions: bool) {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let db = Connection::open(path).unwrap();
            if sessions {
                db.execute_batch("PRAGMA user_version=1;
                    CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT);
                    INSERT INTO meta VALUES('schema_version','1');
                    CREATE TABLE sessions(cutex_session_id TEXT, archive_state TEXT, rev INTEGER, record_json TEXT);
                    CREATE TABLE receipts(map TEXT, key TEXT, rev INTEGER, json TEXT);").unwrap();
            } else {
                db.execute_batch("CREATE TABLE facts(value TEXT);").unwrap();
            }
        }
        fn check(&self, action: &str, relative: &str) -> Candidate {
            recheck(
                &self.root,
                action,
                Path::new(relative),
                30,
                self.now,
                Some("r255"),
                &[],
            )
            .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn start_listing_trace() {
        LISTED_PATHS.with(|paths| *paths.borrow_mut() = Some(Vec::new()));
    }
    fn finish_listing_trace() -> Vec<PathBuf> {
        LISTED_PATHS.with(|paths| paths.borrow_mut().take().unwrap())
    }

    #[test]
    fn selected_log_prepare_and_locked_recheck_never_list_unrelated_log_trees() {
        let f = Fixture::new();
        let path = Path::new("runtime/app-server-journal/selected.jsonl");
        f.file(path.to_str().unwrap(), 40);
        f.file("runtime/diagnostics/unrelated/deep/other.jsonl", 100);
        f.file("runtime/management-v2/orphaned/other.jsonl", 100);
        start_listing_trace();
        let prepared = prepare_recheck(&f.root, "old-logs", path, 30, f.now, None, &[]).unwrap();
        assert!(prepared.candidate.eligible);
        assert!(
            finish_listing_trace().is_empty(),
            "a selected log needs metadata only"
        );
        start_listing_trace();
        assert!(
            recheck_prepared(&f.root, &prepared, Instant::now() + Duration::from_secs(1)).unwrap()
        );
        assert!(
            finish_listing_trace().is_empty(),
            "lock-safe recheck must never enumerate directories"
        );
        f.age(path.to_str().unwrap(), 0);
        assert!(
            !recheck_prepared(&f.root, &prepared, Instant::now() + Duration::from_secs(1)).unwrap()
        );
    }

    #[test]
    fn backup_ticket_detects_counterpart_replacement_and_new_wal_without_reopening_sqlite() {
        let f = Fixture::new();
        let path = Path::new("cutex-sessions.json.bak-20261001");
        f.file(path.to_str().unwrap(), 40);
        f.db("cutex-sessions.sqlite3", true);
        let before = fs::read(f.root.join("cutex-sessions.sqlite3")).unwrap();
        let prepared = prepare_recheck(&f.root, "old-backups", path, 30, f.now, None, &[]).unwrap();
        assert!(prepared.candidate.eligible);
        assert_eq!(
            fs::read(f.root.join("cutex-sessions.sqlite3")).unwrap(),
            before,
            "read-only health must preserve the current DB"
        );
        assert!(
            recheck_prepared(&f.root, &prepared, Instant::now() + Duration::from_secs(1)).unwrap()
        );
        fs::write(
            f.root.join("cutex-sessions.sqlite3-wal"),
            b"new synthetic sidecar",
        )
        .unwrap();
        assert!(
            !recheck_prepared(&f.root, &prepared, Instant::now() + Duration::from_secs(1)).unwrap()
        );
        fs::remove_file(f.root.join("cutex-sessions.sqlite3-wal")).unwrap();
        let prepared = prepare_recheck(&f.root, "old-backups", path, 30, f.now, None, &[]).unwrap();
        assert!(prepared.candidate.eligible);
        fs::write(f.root.join("replacement.tmp"), &before).unwrap();
        fs::rename(
            f.root.join("replacement.tmp"),
            f.root.join("cutex-sessions.sqlite3"),
        )
        .unwrap();
        assert!(
            !recheck_prepared(&f.root, &prepared, Instant::now() + Duration::from_secs(1)).unwrap()
        );
    }

    #[test]
    fn selected_backup_group_lists_only_its_own_leaves() {
        let f = Fixture::new();
        let path = Path::new("codex-home/db-backups/sqlite-1728000000-0");
        f.file(&format!("{}/state_5.sqlite", path.display()), 40);
        f.age(path.to_str().unwrap(), 40);
        f.file("codex-home/db-backups/sqlite-1728000001-0/unknown.json", 40);
        f.file("runtime/diagnostics/unrelated/deep/log.jsonl", 100);
        f.db("codex-home/state_5.sqlite", false);
        start_listing_trace();
        let prepared = prepare_recheck(&f.root, "old-backups", path, 30, f.now, None, &[]).unwrap();
        assert!(prepared.candidate.eligible);
        let listed = finish_listing_trace();
        assert!(!listed.is_empty());
        assert!(
            listed.iter().all(|p| p == path),
            "only selected group may be enumerated: {listed:?}"
        );
        start_listing_trace();
        assert!(
            recheck_prepared(&f.root, &prepared, Instant::now() + Duration::from_secs(1)).unwrap()
        );
        assert!(finish_listing_trace().is_empty());
    }

    #[test]
    fn release_group_snapshot_is_once_and_release_logs_use_24_hours_even_with_90_day_selection() {
        let f = Fixture::new();
        for version in [250, 253, 254, 255] {
            f.file(&format!("logs/management-r{version}.log"), 2);
        }
        let path = Path::new("logs/management-r250.log");
        assert!(is_release_log(path));
        assert!(!is_release_log(Path::new(
            "runtime/diagnostics/management-r250.log"
        )));
        start_listing_trace();
        let prepared =
            prepare_recheck(&f.root, "old-logs", path, 90, f.now, Some("r255"), &[]).unwrap();
        assert!(
            prepared.candidate.eligible,
            "release logs have a 24-hour floor, not the selected normal-log age"
        );
        assert_eq!(finish_listing_trace(), vec![PathBuf::from("logs")]);
        assert!(
            recheck_prepared(&f.root, &prepared, Instant::now() + Duration::from_secs(1)).unwrap()
        );
        fs::remove_file(f.root.join("logs/management-r253.log")).unwrap();
        f.age("logs", 0);
        assert!(
            !recheck_prepared(&f.root, &prepared, Instant::now() + Duration::from_secs(1)).unwrap()
        );
        assert_eq!(
            prepare_recheck(&f.root, "old-logs", path, 90, f.now, Some("r255"), &[])
                .unwrap()
                .candidate
                .code,
            "release_kept"
        );
    }

    #[test]
    fn prepared_ticket_rejects_expired_deadline_another_root_and_new_protection() {
        let f = Fixture::new();
        let other = Fixture::new();
        let path = Path::new("runtime/diagnostics/selected.jsonl");
        f.file(path.to_str().unwrap(), 40);
        let prepared = prepare_recheck(&f.root, "old-logs", path, 30, f.now, None, &[]).unwrap();
        start_listing_trace();
        assert!(!recheck_prepared(
            &f.root,
            &prepared,
            Instant::now() - Duration::from_millis(1)
        )
        .unwrap());
        assert!(!recheck_prepared(
            &other.root,
            &prepared,
            Instant::now() + Duration::from_secs(1)
        )
        .unwrap());
        assert!(finish_listing_trace().is_empty());
        let protected =
            prepare_recheck(&f.root, "old-logs", path, 30, f.now, None, &[path.into()]).unwrap();
        assert_eq!(protected.candidate.code, "protected");
        assert!(
            !recheck_prepared(&f.root, &protected, Instant::now() + Duration::from_secs(1))
                .unwrap()
        );
    }

    #[test]
    fn session_backups_require_the_active_recognized_sqlite_schema() {
        let f = Fixture::new();
        let path = "cutex-sessions.json.pre-sqlite-20261001-2";
        f.file(path, 40);
        assert!(!f.check("old-backups", path).eligible);
        f.db("cutex-sessions.sqlite3", false);
        assert!(
            !f.check("old-backups", path).eligible,
            "generic SQLite is not a sessions store"
        );
        fs::remove_file(f.root.join("cutex-sessions.sqlite3")).unwrap();
        f.db("cutex-sessions.sqlite3", true);
        let before = fs::read(f.root.join(path)).unwrap();
        assert!(f.check("old-backups", path).eligible);
        assert_eq!(
            fs::read(f.root.join(path)).unwrap(),
            before,
            "discovery never modifies the backup"
        );
        for name in [
            "cutex-sessions.json.bak-1728000000",
            "cutex-sessions.json.sentinel-leaked-20261001",
            ".pre-sqlite-20261001",
            ".sentinel-leaked-20261001",
        ] {
            f.file(name, 40);
            assert!(f.check("old-backups", name).eligible, "{name}");
        }
    }

    #[test]
    fn backups_have_a_fourteen_day_floor_and_recheck_age_changes() {
        let f = Fixture::new();
        f.db("cutex-sessions.sqlite3", true);
        let name = "cutex-sessions.json.bak-20261001";
        f.file(name, 14);
        assert!(
            !recheck(&f.root, "old-backups", Path::new(name), 7, f.now, None, &[])
                .unwrap()
                .eligible
        );
        f.age(name, 15);
        assert!(
            recheck(&f.root, "old-backups", Path::new(name), 7, f.now, None, &[])
                .unwrap()
                .eligible
        );
        f.age(name, 0);
        assert!(!f.check("old-backups", name).eligible);
    }

    #[test]
    fn database_corruption_and_busy_checks_fail_closed() {
        let f = Fixture::new();
        let name = "cutex-sessions.json.bak-20261001";
        f.file(name, 40);
        f.file("cutex-sessions.sqlite3", 40);
        assert!(!f.check("old-backups", name).eligible);
        fs::remove_file(f.root.join("cutex-sessions.sqlite3")).unwrap();
        f.db("cutex-sessions.sqlite3", true);
        let locked = Connection::open(f.root.join("cutex-sessions.sqlite3")).unwrap();
        locked.execute_batch("BEGIN EXCLUSIVE;").unwrap();
        assert!(!f.check("old-backups", name).eligible);
        locked.execute_batch("ROLLBACK;").unwrap();
        assert!(f.check("old-backups", name).eligible);
    }

    #[test]
    fn native_writer_groups_require_every_original_and_refuse_unknown_leaves() {
        let f = Fixture::new();
        let group = "codex-home/db-backups/sqlite-1728000000-0";
        f.file(&format!("{group}/state_5.sqlite"), 40);
        f.file(&format!("{group}/logs_2.sqlite-wal"), 40);
        f.age(group, 40);
        f.db("codex-home/state_5.sqlite", false);
        assert!(!f.check("old-backups", group).eligible);
        f.db("codex-home/logs_2.sqlite", false);
        assert!(f.check("old-backups", group).eligible);
        f.file(&format!("{group}/unknown-user-data.json"), 40);
        f.age(group, 40);
        assert!(!f.check("old-backups", group).eligible);
    }

    #[test]
    fn exact_historical_database_groups_and_repair_filenames_are_allowed() {
        let f = Fixture::new();
        f.db("codex-home/logs_2.sqlite", false);
        f.db("codex-home/state_5.sqlite", false);
        for (dir, leaf) in [
            ("logs-db-backup-20260624", "logs_2.sqlite"),
            ("state-db-backup-20261001", "state_5.sqlite"),
            ("sqlite-migration-backup", "state_5.sqlite"),
        ] {
            let relative = format!("codex-home/{dir}");
            f.file(&format!("{relative}/{leaf}"), 40);
            f.age(&relative, 40);
            assert!(f.check("old-backups", &relative).eligible, "{relative}");
        }
        for name in [
            "logs_2.sqlite.bak",
            "logs_2.sqlite.bak-20261001",
            "logs_2.sqlite.bak.20261001",
        ] {
            f.file(&format!("codex-home/{name}"), 40);
            assert!(
                f.check("old-backups", &format!("codex-home/{name}"))
                    .eligible
            );
        }
    }

    #[test]
    fn unknown_backup_names_and_current_database_files_never_qualify() {
        let f = Fixture::new();
        f.db("cutex-sessions.sqlite3", true);
        for name in [
            "my-backup",
            "accounts.json.bak-20261001",
            "cutex-sessions.sqlite3",
            "cutex-sessions.sqlite3-wal",
            "codex-home/state_5.sqlite",
            "codex-home/logs_2.sqlite-wal",
            "codex-home/state-db-backup-surprise",
            "codex-home/db-backups/sqlite-wrong-0",
        ] {
            if !f.root.join(name).exists() {
                f.file(name, 40);
            }
            assert!(!f.check("old-backups", name).eligible, "{name}");
        }
    }

    #[test]
    fn config_and_unique_history_backups_remain_unverified_without_content_reads() {
        let f = Fixture::new();
        f.file("config.before-20261001", 40);
        f.file("config.json", 40); // synthetic only; the catalog never opens either file.
        assert_eq!(
            f.check("old-backups", "config.before-20261001").code,
            "counterpart_unverified"
        );
        f.file("history-repair-backups/session-id/rollout.before.jsonl", 40);
        f.age("history-repair-backups/session-id", 40);
        assert_eq!(
            f.check("old-backups", "history-repair-backups/session-id")
                .code,
            "history_health_unverified"
        );
    }

    #[test]
    fn release_logs_keep_current_and_two_latest_prior_groups_including_rotations() {
        let f = Fixture::new();
        for version in [250, 251, 252, 253, 254, 255, 256] {
            for base in ["agent-bus", "management"] {
                for suffix in ["", ".1", ".2.gz"] {
                    f.file(&format!("logs/{base}-r{version}.log{suffix}"), 40);
                }
            }
        }
        let list = candidates(&f.root, "old-logs", 30, f.now, Some("r255"), &[]).unwrap();
        for item in list {
            assert_eq!(
                item.eligible,
                release_log(&item.relative).unwrap() < 253,
                "{}",
                item.relative.display()
            );
        }
        assert!(candidates(&f.root, "old-logs", 30, f.now, None, &[])
            .unwrap()
            .iter()
            .all(|c| !c.eligible));
    }

    #[test]
    fn old_logs_follow_selected_age_and_release_logs_require_no_recent_write() {
        let f = Fixture::new();
        for prefix in [
            "runtime/diagnostics",
            "runtime/app-server-journal",
            "runtime/management-v2/orphaned",
        ] {
            for age in [0, 1, 7, 8, 30, 31, 90, 91] {
                f.file(&format!("{prefix}/age-{age}.jsonl"), age);
            }
        }
        for days in [7, 30, 90] {
            let list = candidates(&f.root, "old-logs", days, f.now, None, &[]).unwrap();
            assert_eq!(
                list.len(),
                24,
                "all three explicit log roots must be covered"
            );
            for item in list {
                let age: u32 = item
                    .relative
                    .file_stem()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .strip_prefix("age-")
                    .unwrap()
                    .parse()
                    .unwrap();
                assert_eq!(item.eligible, age > days, "{}", item.relative.display());
            }
        }
        for version in [250, 253, 254, 255] {
            f.file(&format!("logs/agent-bus-r{version}.log"), 0);
        }
        assert_eq!(
            f.check("old-logs", "logs/agent-bus-r250.log").code,
            "recent_write"
        );
        f.age("logs/agent-bus-r250.log", 1);
        assert_eq!(
            f.check("old-logs", "logs/agent-bus-r250.log").code,
            "recent_write"
        );
        f.age("logs/agent-bus-r250.log", 2);
        assert!(f.check("old-logs", "logs/agent-bus-r250.log").eligible);
        assert!(candidates(&f.root, "old-logs", 14, f.now, None, &[]).is_err());
    }

    #[test]
    fn protected_paths_cover_children_parents_absolute_paths_and_live_files() {
        let f = Fixture::new();
        let path = "runtime/app-server-journal/current.jsonl";
        f.file(path, 40);
        for protected in [
            PathBuf::from(path),
            f.root.join(path),
            PathBuf::from("runtime/app-server-journal"),
        ] {
            assert_eq!(
                recheck(
                    &f.root,
                    "old-logs",
                    Path::new(path),
                    30,
                    f.now,
                    Some("r255"),
                    &[protected]
                )
                .unwrap()
                .code,
                "protected"
            );
        }
        let group = "codex-home/db-backups/sqlite-1728000000-0";
        f.file(&format!("{group}/state_5.sqlite"), 40);
        f.age(group, 40);
        assert_eq!(
            recheck(
                &f.root,
                "old-backups",
                Path::new(group),
                30,
                f.now,
                None,
                &[PathBuf::from(format!("{group}/state_5.sqlite"))]
            )
            .unwrap()
            .code,
            "protected"
        );
    }

    #[test]
    fn audit_cleanup_records_and_files_outside_the_fixed_roots_are_excluded() {
        let f = Fixture::new();
        for name in [
            "runtime/agent-bus-audit.jsonl",
            "runtime/peers/peer-audit.jsonl",
            "users/audit.jsonl",
            "runtime/storage/cleanups.jsonl",
            "runtime/diagnostics/audit.jsonl",
            "runtime/diagnostics/cleanup-record.jsonl",
            "runtime/claude/adapter.stderr",
            "logs/management-api.log",
            "logs/agent-bus-r1.log.surprise",
        ] {
            f.file(name, 100);
            assert!(!f.check("old-logs", name).eligible, "{name}");
        }
        for path in ["../outside", "/outside", "runtime/../config.json"] {
            assert!(recheck(&f.root, "old-logs", Path::new(path), 30, f.now, None, &[]).is_err());
        }
        assert!(candidates(&f.root, "other", 30, f.now, None, &[]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_targets_are_never_followed_or_counted_as_eligible() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        let other = Fixture::new();
        other.file("private.jsonl", 100);
        fs::create_dir_all(f.root.join("runtime/diagnostics")).unwrap();
        symlink(
            other.root.join("private.jsonl"),
            f.root.join("runtime/diagnostics/linked.jsonl"),
        )
        .unwrap();
        symlink(&other.root, f.root.join("runtime/app-server-journal")).unwrap();
        assert!(
            !f.check("old-logs", "runtime/diagnostics/linked.jsonl")
                .eligible
        );
        assert!(candidates(&f.root, "old-logs", 30, f.now, None, &[])
            .unwrap()
            .iter()
            .all(|c| !c.eligible));
        assert_eq!(
            fs::read(other.root.join("private.jsonl")).unwrap(),
            b"synthetic fixture"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_directory_reparse_targets_are_not_followed() {
        let f = Fixture::new();
        let other = Fixture::new();
        other.file("private.jsonl", 100);
        fs::create_dir_all(f.root.join("runtime")).unwrap();
        // Symlink creation requires developer mode or the Windows symlink privilege.
        if std::os::windows::fs::symlink_dir(&other.root, f.root.join("runtime/diagnostics"))
            .is_err()
        {
            return;
        }
        let list = candidates(&f.root, "old-logs", 30, f.now, None, &[]).unwrap();
        assert!(list.iter().all(|item| !item.eligible));
        assert!(!list
            .iter()
            .any(|item| item.relative.ends_with("private.jsonl")));
        assert_eq!(
            fs::read(other.root.join("private.jsonl")).unwrap(),
            b"synthetic fixture"
        );
    }
}
