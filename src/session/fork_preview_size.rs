//! Bounded, read-only history-file size preview. Never starts a runtime or validates a turn boundary.
use std::collections::HashSet;
use std::fs::{self, File, Metadata};
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{ensure, Context};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;

use crate::session::model::{CutexSessionRecord, CutexSessionRuntimeBackend};

const MAX_ENTRIES: usize = 8192;
const MAX_DEPTH: usize = 32;
const MAX_HEADER: usize = 16 * 1024;
const MAX_DEPENDENCIES: usize = 64;
const TIME_BUDGET: Duration = Duration::from_millis(100);

#[derive(Debug)]
pub struct Estimate {
    pub bytes: Option<u64>,
    pub meaning: String,
}

struct Budget {
    deadline: Instant,
    entries: usize,
    limit: usize,
}
impl Budget {
    fn new() -> Self {
        Self {
            deadline: Instant::now() + TIME_BUDGET,
            entries: 0,
            limit: MAX_ENTRIES,
        }
    }
    fn check(&self) -> anyhow::Result<()> {
        ensure!(
            Instant::now() < self.deadline,
            "100 ms preview time budget exhausted"
        );
        Ok(())
    }
    fn entry(&mut self) -> anyhow::Result<()> {
        self.check()?;
        self.entries += 1;
        ensure!(
            self.entries <= self.limit,
            "preview entry scan limit exceeded"
        );
        Ok(())
    }
}

pub fn estimate(record: &CutexSessionRecord, after_turn_id: Option<&str>) -> Estimate {
    estimate_with(record, after_turn_id, &mut Budget::new())
}

fn estimate_with(
    record: &CutexSessionRecord,
    after: Option<&str>,
    budget: &mut Budget,
) -> Estimate {
    let result = (|| -> anyhow::Result<u64> {
        budget.check()?;
        let bytes = if record.runtime_backend == CutexSessionRuntimeBackend::ClaudeSubscription {
            let binding = record
                .claude_session
                .as_ref()
                .context("Claude frozen account binding unavailable")?;
            let id = binding
                .native_session_id
                .as_deref()
                .context("Claude native session id unavailable")?;
            claude(&binding.deployment.claude_home, id, budget)?
        } else {
            let id = record
                .codex_session_id
                .as_deref()
                .context("Codex native session id unavailable")?;
            let home = match &record.explicit_launch {
                Some(contract) => contract.native_home.clone(),
                None => {
                    // source_home only reads deployment selection and checks path containment;
                    // it does not load credentials, install/adopt a runtime or write a log.
                    let deployment =
                        crate::config::paths::runtime_dir()?.join("light/deployment.json");
                    if let Ok(meta) = fs::symlink_metadata(&deployment) {
                        ensure!(
                            !link(&meta) && meta.is_file() && meta.len() <= MAX_HEADER as u64,
                            "runtime selection unavailable within the preview metadata limit"
                        );
                    }
                    crate::launch::local_deployment::LocalDeployment::source_home()?
                }
            };
            codex(&home, id, budget)?
        };
        budget.check()?;
        Ok(bytes)
    })();
    let boundary = if after.is_some() {
        "Includes later turns beyond the selected afterTurnId; boundary membership was not verified. "
    } else {
        "Fork boundary membership was not verified. "
    };
    let scope = format!("Source history-file upper bound at preview time, including full referenced ancestors/subagent files. {boundary}Excludes newly written fork metadata/projection overhead; this is not the final fork size. Not a token or billing estimate; files may change after preview.");
    match result {
        Ok(bytes) => Estimate { bytes: Some(bytes), meaning: scope },
        Err(error) => Estimate { bytes: None, meaning: format!("History bytes unavailable: {error}. {scope}No partial sum is reported (limits: {MAX_ENTRIES} entries, depth {MAX_DEPTH}, {MAX_DEPENDENCIES} files in a dependency chain, {MAX_HEADER}-byte metadata headers, 100 ms cooperative time budget).") },
    }
}

fn uuid(id: &str) -> anyhow::Result<()> {
    ensure!(
        uuid::Uuid::parse_str(id)?.to_string() == id,
        "native identity is not a canonical UUID"
    );
    Ok(())
}
fn link(meta: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    meta.file_type().is_symlink()
}
fn public_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        return PathBuf::from(crate::agent_management::strip_verbatim_prefix(
            &path.to_string_lossy(),
        ));
    }
    #[cfg(not(windows))]
    {
        path.to_owned()
    }
}

/// A metadata-only path guard. Reject links/reparse points in every component, not only the leaf.
/// Header reads additionally verify the opened descriptor's actual location and identity.
struct Home {
    root: PathBuf,
}
impl Home {
    fn new(path: &Path, budget: &Budget) -> anyhow::Result<Self> {
        ensure!(path.is_absolute(), "native home must be absolute");
        let mut prefix = PathBuf::new();
        for component in path.components() {
            budget.check()?;
            ensure!(
                !matches!(component, Component::ParentDir),
                "parent traversal in native home"
            );
            prefix.push(component.as_os_str());
            if matches!(component, Component::Prefix(_)) {
                continue;
            }
            let meta = fs::symlink_metadata(&prefix)?;
            ensure!(
                !link(&meta) && meta.is_dir(),
                "linked/reparse or unavailable native home"
            );
        }
        Ok(Self {
            root: public_path(&path.canonicalize()?),
        })
    }
    fn metadata(&self, path: &Path, budget: &Budget) -> anyhow::Result<Metadata> {
        budget.check()?;
        let relative = path
            .strip_prefix(&self.root)
            .context("history path is outside native home")?;
        let components: Vec<_> = relative.components().collect();
        ensure!(
            components.len() <= MAX_DEPTH + 2,
            "preview directory depth limit exceeded"
        );
        let mut prefix = self.root.clone();
        let mut last = fs::symlink_metadata(&prefix)?;
        ensure!(
            !link(&last) && last.is_dir(),
            "native home changed to a link/reparse point"
        );
        for (i, component) in components.iter().enumerate() {
            budget.check()?;
            ensure!(
                matches!(component, Component::Normal(_)),
                "history path contains traversal"
            );
            prefix.push(component.as_os_str());
            last = fs::symlink_metadata(&prefix)?;
            ensure!(!link(&last), "history contains a symlink or reparse point");
            if i + 1 != components.len() {
                ensure!(last.is_dir(), "history ancestor is not a directory");
            }
        }
        Ok(last)
    }
    fn file_size(&self, path: &Path, budget: &Budget) -> anyhow::Result<u64> {
        let meta = self.metadata(path, budget)?;
        ensure!(meta.is_file(), "history is not a regular file");
        Ok(meta.len())
    }
    fn read_handle(&self, path: &Path, budget: &Budget) -> anyhow::Result<(File, Metadata)> {
        let before = self.metadata(path, budget)?;
        ensure!(before.is_file(), "native metadata is not a regular file");
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
            };
            options
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
        }
        let file = options.open(path)?;
        let opened = file.metadata()?;
        ensure!(
            !link(&opened) && opened.is_file() && same_file(&before, &opened),
            "rollout changed while opening metadata"
        );
        let actual = public_path(&opened_path(&file)?);
        ensure!(
            actual.starts_with(&self.root),
            "opened rollout resolves outside native home"
        );
        self.metadata(&actual, budget)?;
        Ok((file, before))
    }
    fn header(&self, path: &Path, budget: &Budget) -> anyhow::Result<(Value, u64)> {
        ensure!(
            rollout_ids(path).is_some() && history_location(&self.root, path),
            "refusing to read a non-rollout history header"
        );
        let (file, before) = self.read_handle(path, budget)?;
        let actual = public_path(&opened_path(&file)?);
        ensure!(
            rollout_ids(&actual).is_some() && history_location(&self.root, &actual),
            "opened metadata is not a native rollout inside history directories"
        );
        // Only the first, bounded metadata line is parsed. Prefetch is capped independently of
        // the body size; a gigabyte image-bearing second line is never parsed or loaded.
        let mut reader = BufReader::with_capacity(1024, file.take((MAX_HEADER + 1) as u64));
        let mut header = Vec::new();
        reader.read_until(b'\n', &mut header)?;
        budget.check()?;
        ensure!(
            header.len() <= MAX_HEADER && header.last() == Some(&b'\n'),
            "session_meta header missing or exceeds 16 KiB limit"
        );
        let value: Value =
            serde_json::from_slice(&header).context("invalid session_meta header")?;
        ensure!(
            value["type"] == "session_meta" && value["payload"].is_object(),
            "session_meta header unavailable"
        );
        let after = self.metadata(path, budget)?;
        ensure!(
            same_file(&before, &after)
                && before.len() == after.len()
                && before.modified()? == after.modified()?,
            "rollout changed during preview"
        );
        Ok((value["payload"].clone(), after.len()))
    }
}

#[cfg(unix)]
fn same_file(a: &Metadata, b: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}
#[cfg(windows)]
fn same_file(a: &Metadata, b: &Metadata) -> bool {
    // std Metadata exposes no stable file ID on Windows; descriptor final-path containment and
    // no-delete sharing protect the subsequent read; detect replacement by size and timestamps too.
    a.len() == b.len()
        && a.modified().ok() == b.modified().ok()
        && a.created().ok() == b.created().ok()
}
#[cfg(target_os = "linux")]
fn opened_path(file: &File) -> io::Result<PathBuf> {
    use std::os::fd::AsRawFd;
    fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
}
#[cfg(target_os = "macos")]
fn opened_path(file: &File) -> io::Result<PathBuf> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStringExt;
    let mut bytes = [0u8; libc::PATH_MAX as usize];
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, bytes.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let end = bytes
        .iter()
        .position(|b| *b == 0)
        .ok_or_else(|| io::Error::other("opened history path is too long"))?;
    Ok(std::ffi::OsString::from_vec(bytes[..end].to_vec()).into())
}
#[cfg(windows)]
fn opened_path(file: &File) -> io::Result<PathBuf> {
    use std::os::windows::{ffi::OsStringExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;
    let mut buffer = vec![0u16; 32768];
    let len = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            0,
        )
    };
    if len == 0 {
        return Err(io::Error::last_os_error());
    }
    if len as usize >= buffer.len() {
        return Err(io::Error::other("opened history path is too long"));
    }
    Ok(std::ffi::OsString::from_wide(&buffer[..len as usize]).into())
}
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn opened_path(_file: &File) -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "safe opened-history path validation is unavailable on this platform",
    ))
}

fn walk(
    home: &Home,
    root: &Path,
    budget: &mut Budget,
    mut file: impl FnMut(&Path, u64) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let mut pending = vec![(root.to_owned(), 0usize)];
    while let Some((path, depth)) = pending.pop() {
        budget.check()?;
        ensure!(depth <= MAX_DEPTH, "preview directory depth limit exceeded");
        let meta = home.metadata(&path, budget)?;
        if meta.is_file() {
            file(&path, meta.len())?;
        } else if meta.is_dir() {
            for entry in fs::read_dir(&path)? {
                budget.entry()?;
                pending.push((entry?.path(), depth + 1));
            }
        } else {
            anyhow::bail!("special file in selected history");
        }
    }
    Ok(())
}

fn rollout_ids(path: &Path) -> Option<(String, String)> {
    let name = path
        .file_name()?
        .to_str()?
        .strip_prefix("rollout-")?
        .strip_suffix(".jsonl")?;
    let last = name.get(name.len().checked_sub(36)?..)?;
    uuid(last).ok()?;
    let prefix = name.get(..name.len() - 36)?;
    let thread = if let Some(prefix) = prefix.strip_suffix('_') {
        let thread = prefix.get(prefix.len().checked_sub(36)?..)?;
        uuid(thread).ok()?;
        thread
    } else {
        if !prefix.ends_with('-') {
            return None;
        }
        last
    };
    Some((thread.to_owned(), last.to_owned()))
}

fn history_location(root: &Path, path: &Path) -> bool {
    path.starts_with(root.join("sessions")) || path.starts_with(root.join("archived_sessions"))
}

fn scan_rollouts(home: &Home, budget: &mut Budget) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for dir in ["sessions", "archived_sessions"] {
        let root = home.root.join(dir);
        match home.metadata(&root, budget) {
            Ok(_) => walk(home, &root, budget, |p, _| {
                if rollout_ids(p).is_some() {
                    files.push(p.to_owned());
                }
                Ok(())
            })?,
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(files)
}

fn codex(root: &Path, id: &str, budget: &mut Budget) -> anyhow::Result<u64> {
    uuid(id)?;
    let home = Home::new(root, budget)?;
    let indexed = index_path(&home, id, budget)?;
    let mut scanned = None;
    let source = match indexed {
        Some(path) => path,
        None => {
            let files = scanned.insert(scan_rollouts(&home, budget)?);
            let candidates: Vec<_> = files
                .iter()
                .filter(|p| {
                    p.starts_with(home.root.join("sessions"))
                        && rollout_ids(p).is_some_and(|(thread, _)| thread == id)
                })
                .collect();
            ensure!(
                candidates.len() == 1,
                "current rollout missing or ambiguous without a readable index"
            );
            candidates[0].clone()
        }
    };
    ensure!(
        source.starts_with(home.root.join("sessions")),
        "current rollout is outside active sessions"
    );
    ensure!(
        rollout_ids(&source).is_some_and(|(thread, _)| thread == id),
        "indexed rollout filename does not match the source native identity"
    );
    let mut path = source;
    let mut seen = HashSet::new();
    let mut bytes = 0u64;
    let mut required_prefix = 0u64;
    for dependency in 0..MAX_DEPENDENCIES {
        budget.check()?;
        ensure!(
            seen.insert(path.clone()),
            "cyclic rollout history dependency"
        );
        let (meta, len) = home.header(&path, budget)?;
        let thread = meta["id"]
            .as_str()
            .context("rollout metadata identity unavailable")?;
        uuid(thread)?;
        if dependency == 0 {
            ensure!(
                thread == id,
                "indexed rollout identity does not match the source session"
            );
        }
        ensure!(
            len >= required_prefix,
            "referenced rollout is shorter than its immutable history prefix"
        );
        bytes = bytes
            .checked_add(len)
            .context("history byte sum overflow")?;
        let base = &meta["history_base"];
        if base.is_null() {
            return Ok(bytes);
        }
        let next = base
            .get("rollout_id")
            .or_else(|| base.get("thread_id"))
            .and_then(Value::as_str)
            .context("history_base rollout identity unavailable")?;
        uuid(next)?;
        required_prefix = base["end_byte_offset"]
            .as_u64()
            .context("history_base byte boundary unavailable")?;
        if scanned.is_none() {
            scanned = Some(scan_rollouts(&home, budget)?);
        }
        let candidates: Vec<_> = scanned
            .as_ref()
            .unwrap()
            .iter()
            .filter(|p| rollout_ids(p).is_some_and(|(_, rollout)| rollout == next))
            .collect();
        ensure!(
            candidates.len() == 1,
            "immutable history dependency missing or ambiguous"
        );
        path = candidates[0].clone();
    }
    anyhow::bail!("history dependency limit exceeded")
}

fn index_path(home: &Home, id: &str, budget: &mut Budget) -> anyhow::Result<Option<PathBuf>> {
    let db = home.root.join("state_5.sqlite");
    let before = match home.metadata(&db, budget) {
        Ok(meta) if meta.is_file() => meta,
        Ok(_) => anyhow::bail!("native index is not a regular file"),
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) =>
        {
            return Ok(None)
        }
        Err(error) => return Err(error),
    };
    // Even SQLITE_OPEN_READ_ONLY may create or modify a WAL shared-memory sidecar. Only use
    // immutable mode on a quiescent main file; a live/WAL index falls back to the bounded scan.
    let quiet = || {
        ["-wal", "-shm", "-journal"].iter().all(|suffix| {
            fs::symlink_metadata(home.root.join(format!("state_5.sqlite{suffix}")))
                .is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
        })
    };
    if !quiet() {
        return Ok(None);
    }
    // Hold a no-follow handle while SQLite reads. Linux can address that exact open descriptor,
    // so a pathname replacement cannot redirect even the index read to another file.
    let (index_guard, guarded_metadata) = home.read_handle(&db, budget)?;
    ensure!(
        same_file(&before, &guarded_metadata),
        "native index changed before opening"
    );
    #[cfg(target_os = "linux")]
    let path = {
        use std::os::fd::AsRawFd;
        PathBuf::from(format!("/proc/self/fd/{}", index_guard.as_raw_fd()))
    };
    #[cfg(not(target_os = "linux"))]
    let path = public_path(&db);
    let mut uri = String::from(if path.to_string_lossy().starts_with('/') {
        "file://"
    } else {
        "file:///"
    });
    for b in path.to_string_lossy().replace('\\', "/").bytes() {
        if b.is_ascii_alphanumeric() || b"/:-._~".contains(&b) {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri.push_str("?mode=ro&immutable=1");
    let deadline = budget.deadline;
    let connection = match Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(connection) => connection,
        Err(_) => return Ok(None),
    };
    connection.busy_timeout(Duration::from_millis(5))?;
    unsafe extern "C" fn progress(context: *mut std::ffi::c_void) -> i32 {
        i32::from(Instant::now() >= unsafe { *context.cast::<Instant>() })
    }
    unsafe {
        rusqlite::ffi::sqlite3_limit(
            connection.handle(),
            rusqlite::ffi::SQLITE_LIMIT_LENGTH,
            MAX_HEADER as i32,
        );
        rusqlite::ffi::sqlite3_progress_handler(
            connection.handle(),
            128,
            Some(progress),
            (&deadline as *const Instant).cast_mut().cast(),
        );
    }
    let row: rusqlite::Result<Option<String>> = connection
        .query_row(
            "SELECT rollout_path FROM threads WHERE id=?1 LIMIT 1",
            [id],
            |r| r.get(0),
        )
        .optional();
    drop(connection);
    drop(index_guard);
    budget.check()?;
    let after = home.metadata(&db, budget)?;
    ensure!(
        quiet()
            && same_file(&before, &after)
            && before.len() == after.len()
            && before.modified()? == after.modified()?,
        "native index changed during preview"
    );
    let Some(path) = row.ok().flatten() else {
        return Ok(None);
    };
    let path = public_path(Path::new(&path));
    let path = if path.is_absolute() {
        path
    } else {
        home.root.join(path)
    };
    ensure!(
        path.starts_with(home.root.join("sessions"))
            && rollout_ids(&path).is_some_and(|(thread, _)| thread == id),
        "native index points to an invalid source rollout filename"
    );
    home.file_size(&path, budget)?;
    Ok(Some(path))
}

fn credential(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "auth.json"
            | "accounts.json"
            | "config.json"
            | ".credentials.json"
            | "credentials.json"
            | ".claude.json"
            | "claude-api-account.json"
            | "tokens.json"
            | "token.json"
            | ".env"
            | "id_rsa"
            | "id_ed25519"
    ) || name.ends_with(".pem")
        || name.ends_with(".key")
        || name.starts_with(".env.")
}
fn claude(root: &Path, id: &str, budget: &mut Budget) -> anyhow::Result<u64> {
    uuid(id)?;
    let home = Home::new(root, budget)?;
    let projects = home.root.join("projects");
    ensure!(
        home.metadata(&projects, budget)?.is_dir(),
        "Claude projects directory unavailable"
    );
    let filename = format!("{id}.jsonl");
    let mut transcript = None;
    for entry in fs::read_dir(&projects)? {
        budget.entry()?;
        let entry = entry?;
        let project = entry.path();
        if !home.metadata(&project, budget)?.is_dir() {
            continue;
        }
        let candidate = project.join(&filename);
        match home.metadata(&candidate, budget) {
            Ok(meta) => {
                ensure!(
                    meta.is_file() && transcript.is_none(),
                    "Claude transcript is not a regular file or is ambiguous"
                );
                transcript = Some(candidate);
            }
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) => {}
            Err(error) => return Err(error),
        }
    }
    let transcript = transcript.context("Claude native transcript unavailable")?;
    let mut bytes = home.file_size(&transcript, budget)?;
    let subtree = transcript.parent().unwrap().join(id);
    match home.metadata(&subtree, budget) {
        Ok(meta) => {
            ensure!(
                meta.is_dir(),
                "Claude native history subtree is not a directory"
            );
            walk(&home, &subtree, budget, |path, len| {
                ensure!(
                    !credential(&path.file_name().unwrap_or_default().to_string_lossy()),
                    "credential-like file in Claude native history"
                );
                bytes = bytes
                    .checked_add(len)
                    .context("history byte sum overflow")?;
                Ok(())
            })?;
        }
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) => {}
        Err(error) => return Err(error),
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    const ID: &str = "550e8400-e29b-41d4-a716-446655440000";
    const PARENT: &str = "660e8400-e29b-41d4-a716-446655440001";
    const ROLLOUT: &str = "770e8400-e29b-41d4-a716-446655440002";

    struct Fixture {
        root: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("cutex-fork-preview-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            Self { root }
        }
        fn record(&self) -> CutexSessionRecord {
            let mut record = CutexSessionRecord::new(
                format!("cutex.{ID}"),
                Some(ID.into()),
                "fixture-host".into(),
                self.root.to_string_lossy().into(),
                None,
            )
            .unwrap();
            record.explicit_launch = Some(crate::agent_management::ExplicitLaunchContract {
                version: 4,
                migration_action_id: None,
                native_id: ID.into(),
                native_home: self.root.clone(),
                bundle_manifest: self.root.join("unused-bundle.json"),
                bundle_sha256: crate::role_revision::Sha256::new("0".repeat(64)).unwrap(),
            });
            record
        }
        fn claude_record(&self) -> CutexSessionRecord {
            let mut record = self.record();
            record.runtime_backend = CutexSessionRuntimeBackend::ClaudeSubscription;
            record.claude_session = Some(serde_json::from_value(json!({
                "deployment": {"python":"unused", "adapter_source":"unused", "claude_cli":"unused", "tui_binary":"unused", "claude_home":self.root},
                "thread_id":"adapter-thread", "state_dir":self.root.join("unused-state"), "model":"sonnet", "native_session_id":ID,
            })).unwrap());
            record
        }
        fn rollout(&self, directory: &str, thread: &str, rollout: &str, base: Value) -> PathBuf {
            let ids = if thread == rollout {
                thread.to_owned()
            } else {
                format!("{thread}_{rollout}")
            };
            let path = self
                .root
                .join(directory)
                .join(format!("rollout-2026-10-24T01-00-00-{ids}.jsonl"));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(
                &path,
                format!(
                    "{}\n{{\"body\":\"not parsed by preview\"}}\n",
                    json!({"type":"session_meta","payload":{"id":thread,"history_base":base}})
                ),
            )
            .unwrap();
            path
        }
        fn index(&self, path: &Path) {
            let connection = Connection::open(self.root.join("state_5.sqlite")).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL);",
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO threads VALUES (?1,?2)",
                    rusqlite::params![ID, path.to_str().unwrap()],
                )
                .unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn generous_budget() -> Budget {
        // Test the deterministic work limits independently of machine scheduling. Production
        // estimate() always uses 100 ms; the expired-deadline test covers its cooperative stop.
        Budget {
            deadline: Instant::now() + Duration::from_secs(5),
            entries: 0,
            limit: MAX_ENTRIES,
        }
    }
    fn preview(record: &CutexSessionRecord, after: Option<&str>) -> Estimate {
        estimate_with(record, after, &mut generous_budget())
    }
    fn snapshot(root: &Path) -> Vec<(PathBuf, u64, Option<std::time::SystemTime>)> {
        let mut pending = vec![root.to_owned()];
        let mut result = Vec::new();
        while let Some(path) = pending.pop() {
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() && !link(&meta) {
                pending.extend(fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
            }
            result.push((path, meta.len(), meta.modified().ok()));
        }
        result.sort();
        result
    }
    fn sparse(file: &File, len: u64) {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn DeviceIoControl(
                    handle: *mut std::ffi::c_void,
                    code: u32,
                    input: *const std::ffi::c_void,
                    input_size: u32,
                    output: *mut std::ffi::c_void,
                    output_size: u32,
                    returned: *mut u32,
                    overlapped: *mut std::ffi::c_void,
                ) -> i32;
            }
            let mut returned = 0;
            assert_ne!(
                unsafe {
                    DeviceIoControl(
                        file.as_raw_handle(),
                        0x000900c4,
                        std::ptr::null(),
                        0,
                        std::ptr::null_mut(),
                        0,
                        &mut returned,
                        std::ptr::null_mut(),
                    )
                },
                0,
                "set fixture sparse flag: {}",
                io::Error::last_os_error()
            );
        }
        file.set_len(len).unwrap();
    }

    #[test]
    fn sparse_gigabyte_codex_history_is_statted_with_only_a_bounded_metadata_header() {
        let f = Fixture::new();
        let path = f.rollout("sessions/2026/10/24", ID, ID, Value::Null);
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        sparse(&file, 1 << 30);
        drop(file);
        f.index(&path);
        let before = snapshot(&f.root);
        let estimate = preview(&f.record(), Some("old-boundary-not-validated"));
        assert_eq!(estimate.bytes, Some(1 << 30), "{}", estimate.meaning);
        assert!(estimate.meaning.contains("upper bound"));
        assert!(estimate.meaning.contains("Includes later turns"));
        assert!(estimate
            .meaning
            .contains("boundary membership was not verified"));
        assert!(estimate.meaning.contains("Not a token or billing estimate"));
        assert_eq!(
            snapshot(&f.root),
            before,
            "preview must not create sidecars, state, or logs"
        );
    }

    #[test]
    fn immutable_rollout_ancestors_use_filename_rollout_identity_and_full_file_upper_bounds() {
        let f = Fixture::new();
        let parent = f.rollout("archived_sessions", PARENT, ROLLOUT, Value::Null);
        let source = f.rollout(
            "sessions",
            ID,
            ID,
            json!({"thread_id":ROLLOUT,"end_byte_offset":10,"end_ordinal_exclusive":1}),
        );
        f.index(&source);
        let expected = fs::metadata(&source).unwrap().len() + fs::metadata(&parent).unwrap().len();
        let before = snapshot(&f.root);
        let estimate = preview(&f.record(), Some("old-turn"));
        assert_eq!(estimate.bytes, Some(expected), "{}", estimate.meaning);
        assert_eq!(snapshot(&f.root), before);
    }

    #[test]
    fn readonly_index_selects_current_reverted_rollout_without_changing_directory() {
        let f = Fixture::new();
        let old = f.rollout("sessions", ID, ID, Value::Null);
        let current = f.rollout("sessions", ID, ROLLOUT, Value::Null);
        f.index(&current);
        let before = snapshot(&f.root);
        let estimate = preview(&f.record(), None);
        assert_eq!(
            estimate.bytes,
            Some(fs::metadata(&current).unwrap().len()),
            "{}",
            estimate.meaning
        );
        assert!(old.exists());
        assert_eq!(snapshot(&f.root), before);
    }

    #[test]
    fn corrupted_index_cannot_select_a_credential_named_file_as_history() {
        let f = Fixture::new();
        fs::create_dir(f.root.join("sessions")).unwrap();
        let misplaced = f.root.join("sessions/.credentials.json");
        fs::write(&misplaced, b"fake-never-read-not-a-session-meta").unwrap();
        f.index(&misplaced);
        let before = snapshot(&f.root);
        let estimate = preview(&f.record(), None);
        assert!(estimate.bytes.is_none());
        assert!(
            estimate.meaning.contains("invalid source rollout filename"),
            "{}",
            estimate.meaning
        );
        assert_eq!(snapshot(&f.root), before);
    }

    #[test]
    fn live_index_sidecars_force_metadata_scan_without_shm_or_journal_writes() {
        let f = Fixture::new();
        let source = f.rollout("sessions", ID, ID, Value::Null);
        f.index(&source);
        fs::write(
            f.root.join("state_5.sqlite-wal"),
            b"fixture-not-opened-as-a-wal",
        )
        .unwrap();
        let before = snapshot(&f.root);
        let estimate = preview(&f.record(), None);
        assert_eq!(
            estimate.bytes,
            Some(fs::metadata(&source).unwrap().len()),
            "{}",
            estimate.meaning
        );
        assert_eq!(snapshot(&f.root), before);
        assert!(!f.root.join("state_5.sqlite-shm").exists());
    }

    #[test]
    fn missing_or_unbounded_dependency_metadata_never_returns_a_partial_sum() {
        let f = Fixture::new();
        let source = f.rollout(
            "sessions",
            ID,
            ID,
            json!({"thread_id":PARENT,"end_byte_offset":1}),
        );
        assert!(preview(&f.record(), None).bytes.is_none());
        let parent = f.rollout("archived_sessions", PARENT, PARENT, Value::Null);
        fs::write(
            &parent,
            format!(
                "{}{}\n",
                " ".repeat(MAX_HEADER),
                json!({"type":"session_meta","payload":{"id":PARENT}})
            ),
        )
        .unwrap();
        let estimate = preview(&f.record(), None);
        assert!(estimate.bytes.is_none());
        assert!(estimate.meaning.contains("header"), "{}", estimate.meaning);
        fs::write(&parent, format!("{}\n", json!({"type":"session_meta","payload":{"id":PARENT,"history_base":{"thread_id":ID,"end_byte_offset":1}}}))).unwrap();
        let estimate = preview(&f.record(), None);
        assert!(estimate.bytes.is_none());
        assert!(estimate.meaning.contains("cyclic"), "{}", estimate.meaning);
        assert!(source.exists());
    }

    #[test]
    fn scan_limit_depth_limit_and_expired_deadline_are_explicit_unknowns() {
        let f = Fixture::new();
        f.rollout("sessions", ID, ID, Value::Null);
        fs::write(f.root.join("sessions/extra-1"), b"x").unwrap();
        fs::write(f.root.join("sessions/extra-2"), b"x").unwrap();
        let mut budget = generous_budget();
        budget.limit = 1;
        let estimate = estimate_with(&f.record(), None, &mut budget);
        assert!(estimate.bytes.is_none());
        assert!(
            estimate.meaning.contains("entry scan limit"),
            "{}",
            estimate.meaning
        );
        let mut budget = generous_budget();
        budget.deadline = Instant::now();
        let estimate = estimate_with(&f.record(), None, &mut budget);
        assert!(estimate.bytes.is_none());
        assert!(estimate.meaning.contains("time budget"));
        let mut nested = f.root.join("sessions");
        for _ in 0..MAX_DEPTH + 2 {
            nested.push("n");
        }
        fs::create_dir_all(nested).unwrap();
        let estimate = preview(&f.record(), None);
        assert!(estimate.bytes.is_none());
        assert!(
            estimate.meaning.contains("depth limit"),
            "{}",
            estimate.meaning
        );
    }

    #[test]
    fn claude_stat_only_preview_includes_uuid_subtree_but_not_account_credentials() {
        let f = Fixture::new();
        let project = f.root.join("projects/fixture-slug");
        let sub = project.join(ID).join("subagents/nested");
        fs::create_dir_all(&sub).unwrap();
        // Deliberately not valid JSON. Claude preview must never parse/read native bodies.
        let mut file = File::create(project.join(format!("{ID}.jsonl"))).unwrap();
        file.write_all(b"image-heavy-native-body").unwrap();
        sparse(&file, 1 << 30);
        drop(file);
        fs::write(sub.join("agent-child.jsonl"), b"child-body").unwrap();
        fs::write(f.root.join(".claude.json"), b"fake-never-read").unwrap();
        let before = snapshot(&f.root);
        let estimate = preview(&f.claude_record(), Some("older-turn"));
        assert_eq!(estimate.bytes, Some((1 << 30) + 10), "{}", estimate.meaning);
        assert_eq!(snapshot(&f.root), before);
        fs::write(sub.join("claude-api-account.json"), b"fake-never-read").unwrap();
        assert!(preview(&f.claude_record(), None).bytes.is_none());
    }

    #[test]
    fn ambiguous_missing_native_history_and_short_prefixes_are_unknown() {
        let f = Fixture::new();
        assert!(preview(&f.record(), None).bytes.is_none());
        f.rollout("sessions", ID, ID, Value::Null);
        f.rollout("sessions", ID, ROLLOUT, Value::Null);
        assert!(preview(&f.record(), None).bytes.is_none());
        assert!(preview(&f.claude_record(), None).bytes.is_none());
        let source = f.rollout(
            "sessions",
            ID,
            ID,
            json!({"thread_id":PARENT,"end_byte_offset":u64::MAX}),
        );
        f.rollout("archived_sessions", PARENT, PARENT, Value::Null);
        f.index(&source);
        assert!(preview(&f.record(), None).bytes.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_rollout_and_claude_subtree_are_not_followed() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        let source = f.rollout("sessions", ID, ID, Value::Null);
        let outside = f.root.join("outside");
        fs::create_dir(&outside).unwrap();
        let file = outside.join("external.jsonl");
        fs::rename(&source, &file).unwrap();
        symlink(&file, &source).unwrap();
        assert!(preview(&f.record(), None).bytes.is_none());
        let project = f.root.join("projects/fixture");
        fs::create_dir_all(project.join(ID)).unwrap();
        fs::write(project.join(format!("{ID}.jsonl")), b"body").unwrap();
        symlink(&outside, project.join(ID).join("subagents")).unwrap();
        let before = snapshot(&f.root);
        assert!(preview(&f.claude_record(), None).bytes.is_none());
        assert_eq!(snapshot(&f.root), before);
    }

    #[cfg(windows)]
    #[test]
    fn junction_history_subtree_is_unknown_and_untouched() {
        let f = Fixture::new();
        let project = f.root.join("projects").join("fixture");
        fs::create_dir_all(project.join(ID)).unwrap();
        fs::write(project.join(format!("{ID}.jsonl")), b"body").unwrap();
        let outside = f.root.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("must-not-visit"), b"outside").unwrap();
        let junction = project.join(ID).join("subagents");
        let result = std::process::Command::new("cmd")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&junction)
            .arg(&outside)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let before = snapshot(&f.root);
        assert!(preview(&f.claude_record(), None).bytes.is_none());
        assert_eq!(snapshot(&f.root), before);
        fs::remove_dir(junction).unwrap();
    }
}
