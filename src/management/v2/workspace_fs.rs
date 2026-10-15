//! Workspace file routes: directory listing, file read, fuzzy search and a
//! debounced change-notification SSE stream. All paths are root-relative with
//! forward slashes and confined to the resolved root (see `resolve_rel`).

use std::collections::HashMap;
use std::collections::HashSet;
use std::io::Read;
use std::io::Write;
use std::net::TcpStream;
use std::path::Path;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use base64::Engine;
use serde_json::json;
use serde_json::Value;

use super::workspace::rfc3339;
use super::workspace::ResolvedRoot;
use super::workspace::WorkspaceError;
use super::workspace::WorkspaceResult;

pub const DEFAULT_MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_FILE_BYTES_LIMIT: u64 = 10 * 1024 * 1024;
const SEARCH_DEFAULT_LIMIT: usize = 50;
const SEARCH_MAX_LIMIT: usize = 500;
const FILE_LIST_TTL: Duration = Duration::from_secs(15);
const WALK_FILE_CAP: usize = 200_000;
const WATCH_DEBOUNCE_QUIET: Duration = Duration::from_millis(250);
const WATCH_DEBOUNCE_MAX: Duration = Duration::from_millis(1000);
const WATCH_MAX_PATHS: usize = 256;
const SSE_KEEPALIVE: Duration = Duration::from_secs(15);

/// Resolve a root-relative request path. Returns the canonical absolute path
/// and the normalised relative path (`""` for the root). Rejects absolute
/// paths, drive prefixes, `..`, `.git` components and anything whose
/// canonical form (symlinks/junctions resolved) lies outside the root.
pub fn resolve_rel(root: &ResolvedRoot, raw: &str) -> WorkspaceResult<(PathBuf, String)> {
    let components = normalize_rel(raw)?;
    let mut joined = root.canonical.clone();
    for component in &components {
        joined.push(component);
    }
    let canonical = std::fs::canonicalize(&joined).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            WorkspaceError::new(404, "path_not_found", format!("no such path: {raw}"))
        } else {
            WorkspaceError::internal(error)
        }
    })?;
    let inside = canonical.strip_prefix(&root.canonical).map_err(|_| {
        WorkspaceError::new(
            403,
            "path_outside_root",
            "the path resolves outside the workspace root",
        )
    })?;
    if inside
        .components()
        .any(|component| is_git_dir_name(&component.as_os_str().to_string_lossy()))
    {
        return Err(WorkspaceError::new(
            403,
            "path_forbidden",
            ".git internals are not exposed",
        ));
    }
    Ok((canonical, components.join("/")))
}

fn normalize_rel(raw: &str) -> WorkspaceResult<Vec<String>> {
    if raw.contains('\0') || (cfg!(windows) && raw.contains(':')) {
        return Err(WorkspaceError::bad_request("path must be root-relative"));
    }
    let text = raw.replace('\\', "/");
    if text.starts_with('/') {
        return Err(WorkspaceError::bad_request("path must be root-relative"));
    }
    let mut components = Vec::new();
    for part in text.split('/') {
        match part {
            "" | "." => continue,
            ".." => {
                return Err(WorkspaceError::new(
                    403,
                    "path_outside_root",
                    "path must not contain '..'",
                ))
            }
            name if is_git_dir_name(name) => {
                return Err(WorkspaceError::new(
                    403,
                    "path_forbidden",
                    ".git internals are not exposed",
                ))
            }
            name => components.push(name.to_string()),
        }
    }
    Ok(components)
}

fn is_git_dir_name(name: &str) -> bool {
    name.eq_ignore_ascii_case(".git")
}

fn kind_label(file_type: &std::fs::FileType) -> &'static str {
    if file_type.is_symlink() {
        "symlink"
    } else if file_type.is_dir() {
        "dir"
    } else {
        "file"
    }
}

pub fn dir_resource(
    root: &ResolvedRoot,
    params: &HashMap<String, String>,
) -> WorkspaceResult<Value> {
    let raw = params.get("path").map(String::as_str).unwrap_or("");
    let (dir, rel) = resolve_rel(root, raw)?;
    if !dir.is_dir() {
        return Err(WorkspaceError::new(
            400,
            "not_a_directory",
            "path is not a directory",
        ));
    }
    let read = std::fs::read_dir(&dir).map_err(WorkspaceError::internal)?;
    let mut entries = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_git_dir_name(&name) {
            continue;
        }
        let Ok(link_meta) = entry.path().symlink_metadata() else {
            continue;
        };
        let kind = kind_label(&link_meta.file_type());
        // Symlinks report their target only when it stays inside the root.
        let (target_kind, meta) = if kind == "symlink" {
            match std::fs::canonicalize(entry.path()) {
                Ok(target) if target.starts_with(&root.canonical) => match target.metadata() {
                    Ok(meta) => (Some(if meta.is_dir() { "dir" } else { "file" }), Some(meta)),
                    Err(_) => (None, None),
                },
                _ => (None, None),
            }
        } else {
            (None, Some(link_meta))
        };
        let is_dir = kind == "dir" || target_kind == Some("dir");
        let size = meta
            .as_ref()
            .filter(|meta| meta.is_file())
            .map(|meta| meta.len());
        let modified = meta
            .as_ref()
            .and_then(|meta| meta.modified().ok())
            .map(rfc3339);
        let path = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        entries.push((is_dir, name, path, kind, target_kind, size, modified));
    }
    let names: Vec<&str> = entries.iter().map(|entry| entry.1.as_str()).collect();
    let ignored = super::workspace_git::check_ignored(&dir, &names);
    entries.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| left.1.to_lowercase().cmp(&right.1.to_lowercase()))
            .then_with(|| left.1.cmp(&right.1))
    });
    let entries: Vec<Value> = entries
        .into_iter()
        .map(|(_, name, path, kind, target_kind, size, modified)| {
            let is_ignored = ignored.contains(&name);
            json!({
                "name": name,
                "path": path,
                "kind": kind,
                "targetKind": target_kind,
                "size": size,
                "modifiedAt": modified,
                "ignored": is_ignored,
            })
        })
        .collect();
    Ok(json!({
        "root": root.display,
        "path": rel,
        "entries": entries,
    }))
}

fn mime_for(path: &str) -> Option<&'static str> {
    let ext = path.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "avif" => "image/avif",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "md" | "markdown" => "text/markdown",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" | "cjs" => "text/javascript",
        "json" => "application/json",
        "csv" => "text/csv",
        "xml" => "application/xml",
        "txt" | "log" => "text/plain",
        _ => return None,
    })
}

fn always_binary(mime: Option<&str>) -> bool {
    matches!(mime, Some(mime) if (mime.starts_with("image/") && mime != "image/svg+xml") || mime == "application/pdf")
}

pub fn file_resource(
    root: &ResolvedRoot,
    params: &HashMap<String, String>,
) -> WorkspaceResult<Value> {
    let raw = params
        .get("path")
        .filter(|path| !path.is_empty())
        .ok_or_else(|| WorkspaceError::bad_request("path is required"))?;
    let max_bytes = max_bytes_param(params)?;
    let (file, rel) = resolve_rel(root, raw)?;
    let meta = file.metadata().map_err(WorkspaceError::internal)?;
    if !meta.is_file() {
        return Err(WorkspaceError::new(
            400,
            "not_a_file",
            "path is not a regular file",
        ));
    }
    file_content_value(&file, &rel, &meta, max_bytes)
}

/// `maxBytes` query parameter (default [`DEFAULT_MAX_FILE_BYTES`], at most
/// [`MAX_FILE_BYTES_LIMIT`]).
pub fn max_bytes_param(params: &HashMap<String, String>) -> WorkspaceResult<u64> {
    match params.get("maxBytes") {
        None => Ok(DEFAULT_MAX_FILE_BYTES),
        Some(value) => value
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0 && *value <= MAX_FILE_BYTES_LIMIT)
            .ok_or_else(|| {
                WorkspaceError::bad_request(format!(
                    "maxBytes must be between 1 and {MAX_FILE_BYTES_LIMIT}"
                ))
            }),
    }
}

/// The shared `fs/file` body for a resolved regular file: `{path, size,
/// modifiedAt, encoding: utf8|base64, content, truncated, mime}`. `label` is
/// the reported `path`; its extension picks the mime.
pub fn file_content_value(
    file: &Path,
    label: &str,
    meta: &std::fs::Metadata,
    max_bytes: u64,
) -> WorkspaceResult<Value> {
    let mut bytes = Vec::with_capacity(meta.len().min(max_bytes) as usize);
    std::fs::File::open(file)
        .and_then(|handle| handle.take(max_bytes).read_to_end(&mut bytes))
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::PermissionDenied => {
                WorkspaceError::new(403, "permission_denied", "the file cannot be read")
            }
            _ => WorkspaceError::internal(error),
        })?;
    let truncated = meta.len() > bytes.len() as u64;
    let mime_hint = mime_for(label);
    let text = if always_binary(mime_hint) || bytes[..bytes.len().min(8192)].contains(&0) {
        None
    } else {
        match std::str::from_utf8(&bytes) {
            Ok(text) => Some(text.to_string()),
            // A multi-byte character cut by the size cap is still text.
            Err(error) if truncated && error.error_len().is_none() => {
                Some(String::from_utf8_lossy(&bytes[..error.valid_up_to()]).into_owned())
            }
            Err(_) => None,
        }
    };
    let (encoding, content, mime) = match text {
        Some(text) => ("utf8", text, mime_hint.unwrap_or("text/plain")),
        None => (
            "base64",
            base64::engine::general_purpose::STANDARD.encode(&bytes),
            mime_hint.unwrap_or("application/octet-stream"),
        ),
    };
    Ok(json!({
        "path": label,
        "size": meta.len(),
        "modifiedAt": meta.modified().ok().map(rfc3339),
        "encoding": encoding,
        "content": content,
        "truncated": truncated,
        "mime": mime,
    }))
}

// ---------------------------------------------------------------- search

struct CachedList {
    at: Instant,
    files: Arc<Vec<String>>,
}

fn file_list_cache() -> &'static Mutex<HashMap<PathBuf, CachedList>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedList>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn invalidate_file_list(root: &Path) {
    if let Ok(mut cache) = file_list_cache().lock() {
        cache.remove(root);
    }
}

/// Non-ignored files under the root (root-relative, forward slashes).
pub fn file_list(root: &ResolvedRoot) -> Arc<Vec<String>> {
    if let Ok(cache) = file_list_cache().lock() {
        if let Some(cached) = cache.get(&root.canonical) {
            if cached.at.elapsed() < FILE_LIST_TTL {
                return cached.files.clone();
            }
        }
    }
    let files = Arc::new(
        super::workspace_git::list_files(&root.canonical)
            .unwrap_or_else(|| walk_files(&root.canonical)),
    );
    if let Ok(mut cache) = file_list_cache().lock() {
        cache.insert(
            root.canonical.clone(),
            CachedList {
                at: Instant::now(),
                files: files.clone(),
            },
        );
    }
    files
}

fn walk_files(root: &Path) -> Vec<String> {
    let mut files = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_git_dir_name(&name) {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            if file_type.is_dir() {
                stack.push((entry.path(), path));
            } else if !file_type.is_symlink() || entry.path().is_file() {
                files.push(path);
                if files.len() >= WALK_FILE_CAP {
                    return files;
                }
            }
        }
    }
    files
}

pub fn search_resource(
    root: &ResolvedRoot,
    params: &HashMap<String, String>,
) -> WorkspaceResult<Value> {
    let query: String = params
        .get("q")
        .map(|query| query.chars().filter(|ch| !ch.is_whitespace()).collect())
        .unwrap_or_default();
    let limit = match params.get("limit") {
        None => SEARCH_DEFAULT_LIMIT,
        Some(value) => value
            .parse::<usize>()
            .ok()
            .filter(|value| *value > 0 && *value <= SEARCH_MAX_LIMIT)
            .ok_or_else(|| {
                WorkspaceError::bad_request(format!(
                    "limit must be between 1 and {SEARCH_MAX_LIMIT}"
                ))
            })?,
    };
    let files = file_list(root);
    let results = fuzzy_search(&files, &query, limit, |path| {
        root.canonical.join(path).exists()
    });
    Ok(json!({
        "query": query,
        "results": results
            .into_iter()
            .map(|(path, score)| json!({"path": path, "score": score}))
            .collect::<Vec<_>>(),
    }))
}

/// Rank `files` against `query` (case-insensitive subsequence with
/// segment/consecutive bonuses). `exists` filters stale cache entries.
pub fn fuzzy_search(
    files: &[String],
    query: &str,
    limit: usize,
    exists: impl Fn(&str) -> bool,
) -> Vec<(String, i64)> {
    let query: Vec<char> = query.replace('\\', "/").to_lowercase().chars().collect();
    let mut scored: Vec<(i64, &String)> = if query.is_empty() {
        files.iter().map(|path| (0, path)).collect()
    } else {
        files
            .iter()
            .filter_map(|path| fuzzy_score(&query, path).map(|score| (score, path)))
            .collect()
    };
    scored.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| left.1.len().cmp(&right.1.len()))
            .then_with(|| left.1.cmp(right.1))
    });
    scored
        .into_iter()
        .filter(|(_, path)| exists(path))
        .take(limit)
        .map(|(score, path)| (path.clone(), score))
        .collect()
}

const NEG: i64 = i64::MIN / 4;

pub fn fuzzy_score(query: &[char], path: &str) -> Option<i64> {
    let original: Vec<char> = path.chars().collect();
    let lower: Vec<char> = path.to_lowercase().chars().collect();
    if lower.len() != original.len() || query.len() > lower.len() {
        return None;
    }
    // Fast subsequence rejection.
    let mut position = 0;
    for ch in query {
        position = position
            + lower[position..]
                .iter()
                .position(|candidate| candidate == ch)?
            + 1;
    }
    let base_start = path
        .rfind('/')
        .map(|index| path[..index].chars().count() + 1)
        .unwrap_or(0);
    let bonus = |index: usize| -> i64 {
        if index == 0 {
            return 8;
        }
        let previous = original[index - 1];
        let current = original[index];
        let mut value = 0;
        if previous == '/' {
            value += 8;
        } else if matches!(previous, '_' | '-' | '.' | ' ') {
            value += 6;
        } else if previous.is_lowercase() && current.is_uppercase() {
            value += 6;
        }
        if index >= base_start {
            value += 2;
        }
        value
    };
    let width = lower.len();
    let mut previous_row = vec![NEG; width];
    for (query_index, query_char) in query.iter().enumerate() {
        let mut row = vec![NEG; width];
        let mut running = NEG;
        for column in 0..width {
            if query_index > 0 && column > 0 {
                running = (running - 1).max(previous_row[column - 1]);
            }
            if lower[column] != *query_char {
                continue;
            }
            let base = 16 + bonus(column);
            row[column] = if query_index == 0 {
                base - (column as i64).min(16) / 4
            } else if column == 0 {
                NEG
            } else {
                let consecutive = if previous_row[column - 1] > NEG {
                    previous_row[column - 1] + 10
                } else {
                    NEG
                };
                let gapped = if running > NEG { running - 2 } else { NEG };
                let best = consecutive.max(gapped);
                if best > NEG {
                    base + best
                } else {
                    NEG
                }
            };
        }
        previous_row = row;
    }
    let (end, best) = previous_row
        .iter()
        .enumerate()
        .max_by_key(|(_, score)| **score)
        .filter(|(_, score)| **score > NEG)?;
    let mut score = *best - (width as i64) / 8;
    if end >= base_start {
        score += 4;
    }
    Some(score)
}

// ---------------------------------------------------------------- watch

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchBatch {
    pub paths: Vec<String>,
    pub git_changed: bool,
    pub overflow: bool,
}

struct RootWatch {
    subscribers: Mutex<Vec<mpsc::SyncSender<WatchBatch>>>,
    _watcher: Mutex<Option<notify::RecommendedWatcher>>,
}

fn watch_hub() -> &'static Mutex<HashMap<PathBuf, Arc<RootWatch>>> {
    static HUB: OnceLock<Mutex<HashMap<PathBuf, Arc<RootWatch>>>> = OnceLock::new();
    HUB.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Subscribe to debounced changes under `root`; the watcher starts lazily
/// with the first subscriber and stops once every receiver is dropped and
/// `prune_watch` runs.
pub fn subscribe_watch(root: &Path) -> WorkspaceResult<mpsc::Receiver<WatchBatch>> {
    let (sender, receiver) = mpsc::sync_channel(64);
    let mut hub = watch_hub()
        .lock()
        .map_err(|_| WorkspaceError::internal("watch hub poisoned"))?;
    if let Some(existing) = hub.get(root) {
        existing
            .subscribers
            .lock()
            .map_err(|_| WorkspaceError::internal("watch poisoned"))?
            .push(sender);
        return Ok(receiver);
    }
    use notify::Watcher;
    let (raw_tx, raw_rx) = mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if event.as_ref().is_ok_and(|event| {
            !is_content_change(&event.kind) || is_directory_metadata_touch(event)
        }) {
            return;
        }
        let _ = raw_tx.send(event);
    })
    .map_err(|error| WorkspaceError::new(503, "watch_unavailable", error.to_string()))?;
    watcher
        .watch(root, notify::RecursiveMode::Recursive)
        .map_err(|error| WorkspaceError::new(503, "watch_unavailable", error.to_string()))?;
    let entry = Arc::new(RootWatch {
        subscribers: Mutex::new(vec![sender]),
        _watcher: Mutex::new(Some(watcher)),
    });
    hub.insert(root.to_path_buf(), entry.clone());
    let root = root.to_path_buf();
    let weak = Arc::downgrade(&entry);
    std::thread::Builder::new()
        .name("cutex-fs-watch".to_string())
        .spawn(move || debounce_loop(root, raw_rx, weak))
        .map_err(WorkspaceError::internal)?;
    Ok(receiver)
}

/// Whether a raw watcher event can change what the workspace routes show.
/// Opening or reading a file cannot: notify's inotify backend reports opens
/// (`IN_OPEN`, including directories), and `GET git/status` itself opens
/// every untracked file (line counts) and directory (`git status`). Reported
/// as changes, those opens fed a loop on the Linux host (2026-10-09): every status
/// request produced a `changed` batch, the desktop reloaded the status 300 ms
/// later, and so on, about twice a second (~15 % CPU of the management
/// process) with nothing changing on disk. A closed write handle is kept.
fn is_content_change(kind: &notify::EventKind) -> bool {
    use notify::event::AccessKind;
    use notify::event::AccessMode;
    match kind {
        notify::EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        notify::EventKind::Access(_) => false,
        _ => true,
    }
}

/// A "modified" event on a directory itself (not a rename). Windows reports
/// one when a file inside is merely read or the directory is listed (access
/// time / directory metadata), which would feed the same loop. Real content
/// changes inside a directory also report the entry itself (create, remove,
/// rename, or a modify on the file), so nothing is lost by skipping these.
fn is_directory_metadata_touch(event: &notify::Event) -> bool {
    matches!(
        event.kind,
        notify::EventKind::Modify(kind) if !matches!(kind, notify::event::ModifyKind::Name(_))
    ) && !event.paths.is_empty()
        && event.paths.iter().all(|path| path.is_dir())
}

fn debounce_loop(
    root: PathBuf,
    raw: mpsc::Receiver<notify::Result<notify::Event>>,
    entry: std::sync::Weak<RootWatch>,
) {
    while let Ok(first) = raw.recv() {
        let started = Instant::now();
        let mut events = vec![first];
        while started.elapsed() < WATCH_DEBOUNCE_MAX {
            match raw.recv_timeout(WATCH_DEBOUNCE_QUIET) {
                Ok(event) => events.push(event),
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
        let paths = events
            .into_iter()
            .filter_map(Result::ok)
            .flat_map(|event| event.paths)
            .collect::<Vec<_>>();
        let batch = batch_from_paths(&root, &paths);
        if batch.paths.is_empty() && !batch.git_changed && !batch.overflow {
            continue;
        }
        if !batch.paths.is_empty() || batch.overflow {
            invalidate_file_list(&root);
        }
        let Some(live) = entry.upgrade() else {
            return;
        };
        if let Ok(mut subscribers) = live.subscribers.lock() {
            subscribers.retain(|subscriber| match subscriber.try_send(batch.clone()) {
                Ok(()) | Err(mpsc::TrySendError::Full(_)) => true,
                Err(mpsc::TrySendError::Disconnected(_)) => false,
            });
        };
    }
}

/// Convert raw absolute event paths into a root-relative batch; `.git`
/// internals only set `git_changed`.
pub fn batch_from_paths(root: &Path, paths: &[PathBuf]) -> WatchBatch {
    let mut seen = HashSet::new();
    let mut batch = WatchBatch {
        paths: Vec::new(),
        git_changed: false,
        overflow: false,
    };
    // Event paths may lack the verbatim prefix the canonical root carries.
    let plain_root = PathBuf::from(super::workspace::display_path(root));
    for path in paths {
        let Ok(rel) = path
            .strip_prefix(root)
            .or_else(|_| path.strip_prefix(&plain_root))
        else {
            continue;
        };
        let parts: Vec<String> = rel
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect();
        if parts.is_empty() {
            continue;
        }
        if parts.iter().any(|part| is_git_dir_name(part)) {
            batch.git_changed = true;
            continue;
        }
        let rel = parts.join("/");
        if seen.insert(rel.clone()) {
            if batch.paths.len() >= WATCH_MAX_PATHS {
                batch.overflow = true;
            } else {
                batch.paths.push(rel);
            }
        }
    }
    batch
}

fn prune_watch(root: &Path) {
    let Ok(mut hub) = watch_hub().lock() else {
        return;
    };
    let empty = hub.get(root).is_some_and(|entry| {
        entry
            .subscribers
            .lock()
            .map(|mut subscribers| {
                // Probe with an empty batch (ignored by live streams) to drop
                // senders whose receiver is gone.
                subscribers.retain(|subscriber| {
                    !matches!(
                        subscriber.try_send(WatchBatch {
                            paths: Vec::new(),
                            git_changed: false,
                            overflow: false,
                        }),
                        Err(mpsc::TrySendError::Disconnected(_))
                    )
                });
                subscribers.is_empty()
            })
            .unwrap_or(true)
    });
    if empty {
        hub.remove(root);
    }
}

/// `GET {scope}/fs/watch`: SSE stream. Events:
/// `ready` `{scope, rootId, root}` once, then `changed`
/// `{scope, rootId, paths[], gitChanged, overflow}`; `: keepalive` comments.
pub fn watch_stream(stream: &mut TcpStream, root: &ResolvedRoot) -> anyhow::Result<()> {
    let receiver = match subscribe_watch(&root.canonical) {
        Ok(receiver) => receiver,
        Err(error) => return super::workspace::write_error(stream, &error),
    };
    stream.set_write_timeout(Some(Duration::from_secs(10))).ok();
    let scope = root.scope.kind.label();
    let root_id = root.scope.id.clone();
    let result = (|| -> anyhow::Result<()> {
        stream.write_all(
            crate::http::server::sse_response_head().as_bytes(),
        )?;
        write_sse(
            stream,
            "ready",
            &json!({"scope": scope, "rootId": root_id, "root": root.display}),
        )?;
        let mut watch = crate::management::listeners::RevocationWatch::for_current_request();
        let mut last_write = std::time::Instant::now();
        loop {
            if let Some(token_id) = watch.revoked() {
                crate::management::listeners::write_revoked_frame(stream, &token_id)?;
                return Ok(());
            }
            match receiver.recv_timeout(crate::management::listeners::STREAM_TICK) {
                Ok(batch) => {
                    last_write = std::time::Instant::now();
                    // Empty batches are prune probes.
                    if batch.paths.is_empty() && !batch.git_changed && !batch.overflow {
                        continue;
                    }
                    write_sse(
                        stream,
                        "changed",
                        &json!({
                            "scope": scope,
                            "rootId": root_id,
                            "paths": batch.paths,
                            "gitChanged": batch.git_changed,
                            "overflow": batch.overflow,
                        }),
                    )?;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if last_write.elapsed() >= SSE_KEEPALIVE {
                        stream.write_all(b": keepalive\n\n")?;
                        stream.flush()?;
                        last_write = std::time::Instant::now();
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }
    })();
    drop(receiver);
    prune_watch(&root.canonical);
    let _ = stream.shutdown(std::net::Shutdown::Both);
    // A dropped client is the normal end of an SSE stream.
    let _ = result;
    Ok(())
}

pub fn write_sse(stream: &mut impl Write, event: &str, data: &Value) -> anyhow::Result<()> {
    write!(
        stream,
        "event: {event}\ndata: {}\n\n",
        serde_json::to_string(data)?
    )?;
    stream.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::workspace::Scope;
    use super::super::workspace::ScopeKind;
    use super::*;

    fn temp_root(name: &str) -> (PathBuf, ResolvedRoot) {
        let dir = std::env::temp_dir().join(format!(
            "cutex-ws-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let root = ResolvedRoot::from_dir(
            Scope {
                kind: ScopeKind::Project,
                id: "p".into(),
            },
            &dir,
        )
        .unwrap();
        (dir, root)
    }

    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn resolve_rejects_escapes_and_git() {
        let (dir, root) = temp_root("resolve");
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join("a/b/f.txt"), "x").unwrap();
        assert_eq!(resolve_rel(&root, "a/b/f.txt").unwrap().1, "a/b/f.txt");
        assert_eq!(resolve_rel(&root, "./a\\b").unwrap().1, "a/b");
        assert_eq!(resolve_rel(&root, "").unwrap().1, "");
        assert_eq!(
            resolve_rel(&root, "../x").unwrap_err().code,
            "path_outside_root"
        );
        assert_eq!(
            resolve_rel(&root, "a/../../x").unwrap_err().code,
            "path_outside_root"
        );
        assert_eq!(
            resolve_rel(&root, "/etc").unwrap_err().code,
            "invalid_request"
        );
        #[cfg(windows)]
        assert_eq!(
            resolve_rel(&root, "C:/Windows").unwrap_err().code,
            "invalid_request"
        );
        assert_eq!(
            resolve_rel(&root, ".git/config").unwrap_err().code,
            "path_forbidden"
        );
        assert_eq!(
            resolve_rel(&root, ".GIT").unwrap_err().code,
            "path_forbidden"
        );
        assert_eq!(
            resolve_rel(&root, "nope").unwrap_err().code,
            "path_not_found"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn resolve_rejects_symlink_escape() {
        let (dir, root) = temp_root("symlink");
        let (outside, _) = temp_root("outside");
        std::fs::write(outside.join("secret.txt"), "s").unwrap();
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&outside, dir.join("link")).is_ok();
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_dir(&outside, dir.join("link")).is_ok()
            || std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(dir.join("link"))
                .arg(&outside)
                .output()
                .map(|output| output.status.success())
                .unwrap_or(false);
        if linked {
            assert_eq!(
                resolve_rel(&root, "link/secret.txt").unwrap_err().code,
                "path_outside_root"
            );
            let listing = dir_resource(&root, &params(&[])).unwrap();
            let entry = &listing["entries"][0];
            assert_eq!(entry["kind"], "symlink");
            assert_eq!(entry["targetKind"], Value::Null);
        }
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(outside);
    }

    #[test]
    fn dir_lists_dirs_first_hides_git_and_marks_ignored() {
        let (dir, root) = temp_root("dir");
        let git_ok = std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .current_dir(&dir)
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        std::fs::write(dir.join(".gitignore"), "*.log\nbuild/\n").unwrap();
        std::fs::create_dir_all(dir.join("build")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("b.txt"), "hello").unwrap();
        std::fs::write(dir.join("a.log"), "").unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let listing = dir_resource(&root, &params(&[])).unwrap();
        let names: Vec<&str> = listing["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["build", "src", ".gitignore", "a.log", "b.txt"]);
        let entry = |name: &str| {
            listing["entries"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["name"] == name)
                .unwrap()
                .clone()
        };
        assert_eq!(entry("b.txt")["size"], 5);
        assert_eq!(entry("b.txt")["kind"], "file");
        assert_eq!(entry("src")["kind"], "dir");
        if git_ok {
            assert_eq!(entry("a.log")["ignored"], true);
            assert_eq!(entry("build")["ignored"], true);
            assert_eq!(entry("b.txt")["ignored"], false);
        }
        let sub = dir_resource(&root, &params(&[("path", "src")])).unwrap();
        assert_eq!(sub["path"], "src");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn file_reads_text_binary_and_truncates() {
        let (dir, root) = temp_root("file");
        std::fs::write(dir.join("t.md"), "# héllo").unwrap();
        std::fs::write(dir.join("bin.dat"), [0u8, 1, 2, 3]).unwrap();
        std::fs::write(dir.join("i.png"), b"PNGDATA").unwrap();
        let text = file_resource(&root, &params(&[("path", "t.md")])).unwrap();
        assert_eq!(text["encoding"], "utf8");
        assert_eq!(text["content"], "# héllo");
        assert_eq!(text["mime"], "text/markdown");
        assert_eq!(text["truncated"], false);
        let binary = file_resource(&root, &params(&[("path", "bin.dat")])).unwrap();
        assert_eq!(binary["encoding"], "base64");
        assert_eq!(binary["content"], "AAECAw==");
        assert_eq!(binary["mime"], "application/octet-stream");
        let image = file_resource(&root, &params(&[("path", "i.png")])).unwrap();
        assert_eq!(image["encoding"], "base64");
        assert_eq!(image["mime"], "image/png");
        // Cut inside the two-byte 'é'.
        let cut = file_resource(&root, &params(&[("path", "t.md"), ("maxBytes", "4")])).unwrap();
        assert_eq!(cut["encoding"], "utf8");
        assert_eq!(cut["content"], "# h");
        assert_eq!(cut["truncated"], true);
        assert_eq!(cut["size"], 8);
        assert_eq!(
            file_resource(&root, &params(&[("path", "t.md"), ("maxBytes", "0")]))
                .unwrap_err()
                .code,
            "invalid_request"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn fuzzy_prefers_basename_and_consecutive_matches() {
        let files: Vec<String> = [
            "src/management/v2/server.rs",
            "src/management/v2/session.rs",
            "docs/servers/readme.md",
            "src/lib.rs",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let results = fuzzy_search(&files, "server", 10, |_| true);
        assert_eq!(results[0].0, "src/management/v2/server.rs");
        assert!(results.iter().all(|(path, _)| path != "src/lib.rs"));
        let results = fuzzy_search(&files, "v2sess", 10, |_| true);
        assert_eq!(results[0].0, "src/management/v2/session.rs");
        assert!(fuzzy_search(&files, "zzz", 10, |_| true).is_empty());
        assert_eq!(fuzzy_search(&files, "", 2, |_| true).len(), 2);
    }

    #[test]
    fn search_respects_gitignore_or_walks() {
        let (dir, root) = temp_root("search");
        let _ = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .status();
        std::fs::write(dir.join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir_all(dir.join("ignored")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("ignored/target.rs"), "").unwrap();
        std::fs::write(dir.join("src/target.rs"), "").unwrap();
        let result = search_resource(&root, &params(&[("q", "target")])).unwrap();
        let paths: Vec<&str> = result["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["path"].as_str().unwrap())
            .collect();
        assert!(paths.contains(&"src/target.rs"));
        if super::super::workspace_git::list_files(&root.canonical).is_some() {
            assert!(!paths.contains(&"ignored/target.rs"));
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn batch_filters_git_and_dedupes() {
        let root = PathBuf::from("/r");
        let batch = batch_from_paths(
            &root,
            &[
                PathBuf::from("/r/a.txt"),
                PathBuf::from("/r/a.txt"),
                PathBuf::from("/r/.git/index"),
                PathBuf::from("/elsewhere/x"),
                PathBuf::from("/r/d/e"),
            ],
        );
        assert_eq!(batch.paths, vec!["a.txt", "d/e"]);
        assert!(batch.git_changed);
        assert!(!batch.overflow);
    }

    #[test]
    fn watcher_reports_changes() {
        let (dir, root) = temp_root("watch");
        let receiver = subscribe_watch(&root.canonical).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        std::fs::write(dir.join("new.txt"), "x").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut found = false;
        while Instant::now() < deadline && !found {
            if let Ok(batch) = receiver.recv_timeout(Duration::from_millis(500)) {
                found = batch.paths.iter().any(|path| path == "new.txt");
            }
        }
        assert!(found, "watch event for new.txt");
        drop(receiver);
        prune_watch(&root.canonical);
        assert!(!watch_hub().lock().unwrap().contains_key(&root.canonical));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn only_content_changes_count_as_watch_events() {
        use notify::event::{AccessKind, AccessMode, CreateKind, ModifyKind, RemoveKind};
        use notify::EventKind;
        assert!(!is_content_change(&EventKind::Access(AccessKind::Open(AccessMode::Any))));
        assert!(!is_content_change(&EventKind::Access(AccessKind::Read)));
        assert!(!is_content_change(&EventKind::Access(AccessKind::Close(AccessMode::Read))));
        assert!(!is_content_change(&EventKind::Access(AccessKind::Any)));
        assert!(is_content_change(&EventKind::Access(AccessKind::Close(AccessMode::Write))));
        assert!(is_content_change(&EventKind::Create(CreateKind::File)));
        assert!(is_content_change(&EventKind::Modify(ModifyKind::Any)));
        assert!(is_content_change(&EventKind::Remove(RemoveKind::Any)));
        assert!(is_content_change(&EventKind::Any));

        use notify::event::RenameMode;
        let (dir, _root) = temp_root("dir-touch");
        std::fs::write(dir.join("f.txt"), "x").unwrap();
        let event = |kind, path: PathBuf| notify::Event::new(kind).add_path(path);
        assert!(is_directory_metadata_touch(&event(EventKind::Modify(ModifyKind::Any), dir.clone())));
        assert!(!is_directory_metadata_touch(&event(EventKind::Modify(ModifyKind::Any), dir.join("f.txt"))));
        assert!(!is_directory_metadata_touch(&event(
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            dir.clone()
        )));
        assert!(!is_directory_metadata_touch(&event(EventKind::Create(CreateKind::Folder), dir.clone())));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reading_files_and_listing_dirs_reports_no_change() {
        let (dir, root) = temp_root("watch-read");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/a.txt"), "x\ny\n").unwrap();
        std::fs::write(dir.join("b.txt"), "z").unwrap();
        let receiver = subscribe_watch(&root.canonical).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        // What a status / tree request does: open and read files, list dirs.
        let deadline = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < deadline {
            let _ = std::fs::read(dir.join("sub/a.txt")).unwrap();
            let _ = std::fs::read(dir.join("b.txt")).unwrap();
            let _ = std::fs::read_dir(&dir).unwrap().count();
            let _ = std::fs::read_dir(dir.join("sub")).unwrap().count();
            std::thread::sleep(Duration::from_millis(100));
        }
        let quiet = receiver.recv_timeout(Duration::from_millis(1500));
        assert!(quiet.is_err(), "reads reported as a change: {quiet:?}");
        // A real write still arrives.
        std::fs::write(dir.join("sub/a.txt"), "changed").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut found = false;
        while Instant::now() < deadline && !found {
            if let Ok(batch) = receiver.recv_timeout(Duration::from_millis(500)) {
                found = batch.paths.iter().any(|path| path == "sub/a.txt");
            }
        }
        assert!(found, "watch event for sub/a.txt");
        drop(receiver);
        prune_watch(&root.canonical);
        let _ = std::fs::remove_dir_all(dir);
    }
}
