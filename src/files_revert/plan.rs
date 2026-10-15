use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{content_hash, hash, patch};

pub const MAX_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Change {
    pub path: String,
    pub move_path: Option<String>,
    pub kind: String,
    pub diff: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilePlan {
    pub path: String,
    pub relative_path: Option<String>,
    pub move_path: Option<String>,
    pub action: String,
    pub status: String,
    pub reason: Option<String>,
    pub added: usize,
    pub deleted: usize,
}

#[derive(Clone, Debug)]
pub struct Write {
    pub path: PathBuf,
    pub before: Option<Vec<u8>>,
    pub after: Option<Vec<u8>>,
    pub mode: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub root: PathBuf,
    pub extra_root: Option<PathBuf>,
    pub files: Vec<FilePlan>,
    pub writes: Vec<Write>,
    pub digest: String,
}

impl Plan {
    pub fn blocked(&self) -> bool {
        self.files
            .iter()
            .any(|f| !matches!(f.status.as_str(), "ok" | "alreadyReverted"))
    }
    pub fn applicable(&self) -> bool {
        !self.blocked() && self.files.iter().any(|f| f.status == "ok")
    }
}

pub fn changes(turn: &Value) -> Result<(Vec<Change>, Vec<Value>), String> {
    let items = turn
        .get("items")
        .and_then(Value::as_array)
        .ok_or("turn has no complete items array")?;
    let mut result = Vec::new();
    let mut failed = 0;
    let mut commands = 0;
    let mut subagents = false;
    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("fileChange") => {
                if item.get("status").and_then(Value::as_str) != Some("completed") {
                    failed += 1;
                    continue;
                }
                let list = item
                    .get("changes")
                    .and_then(Value::as_array)
                    .ok_or("fileChange has no changes array")?;
                for change in list {
                    let kind = &change["kind"];
                    result.push(Change {
                        path: change["path"]
                            .as_str()
                            .ok_or("fileChange has no path")?
                            .into(),
                        move_path: kind
                            .get("move_path")
                            .or_else(|| kind.get("movePath"))
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        kind: kind
                            .as_str()
                            .or_else(|| kind.get("type").and_then(Value::as_str))
                            .ok_or("fileChange has no kind")?
                            .into(),
                        diff: change["diff"]
                            .as_str()
                            .ok_or("fileChange has no complete diff")?
                            .into(),
                    });
                }
            }
            Some("commandExecution") => commands += 1,
            Some("collabAgentToolCall") => subagents = true,
            _ => {}
        }
    }
    let mut warnings = Vec::new();
    if commands > 0 {
        warnings.push(json!({"kind":"commandExecutionNotReverted","count":commands}));
    }
    if subagents {
        warnings.push(json!({"kind":"subagentChangesNotIncluded"}));
    }
    if failed > 0 {
        warnings.push(json!({"kind":"failedChangesSkipped","count":failed}));
    }
    Ok((result, warnings))
}

fn path_key(raw: &str) -> String {
    let text = raw.replace('\\', "/");
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text
    }
}

fn symlink(meta: &fs::Metadata) -> bool {
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

/// Absolute history paths are accepted only inside the canonical workspace.
/// Reject links at every existing component, including junctions on Windows.
pub fn resolve(root: &Path, raw: &str) -> Result<PathBuf, (&'static str, String)> {
    let reject = |reason: &str| ("outsideWorkspace", reason.to_owned());
    if root.components().any(|part| {
        part.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(".git")
    }) {
        return Err(reject("Git internals cannot be a revert workspace"));
    }
    let canonical = fs::canonicalize(root).map_err(|_| reject("workspace root is unavailable"))?;
    if path_key(&canonical.to_string_lossy()) != path_key(&root.to_string_lossy()) {
        return Err(reject("workspace root binding changed"));
    }
    if raw.is_empty() || raw.contains('\0') {
        return Err(reject("invalid empty path"));
    }
    let normalized = raw.replace('\\', "/");
    let path = Path::new(&normalized);
    let relative = if path.is_absolute() {
        // canonical roots on Windows have the extended-length prefix.
        let root_text = root.to_string_lossy().replace('\\', "/");
        let root_text = root_text.strip_prefix("//?/").unwrap_or(&root_text);
        let prefix = format!("{}/", root_text.trim_end_matches('/'));
        if !path_key(&normalized).starts_with(&path_key(&prefix)) {
            return Err(reject("path is outside the workspace"));
        }
        PathBuf::from(&normalized[prefix.len()..])
    } else {
        path.to_owned()
    };
    let mut result = root.to_owned();
    // Deepest existing component, and the not-yet-existing names below it.
    let mut existing = root.to_owned();
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    for component in relative.components() {
        if component == Component::CurDir {
            continue;
        }
        let Component::Normal(name) = component else {
            return Err(reject("path contains a non-normal component"));
        };
        let name_text = name.to_string_lossy();
        #[cfg(windows)]
        {
            let base = name_text
                .split('.')
                .next()
                .unwrap_or("")
                .to_ascii_uppercase();
            let device = matches!(
                base.as_str(),
                "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
            ) || (base.len() == 4
                && (base.starts_with("COM") || base.starts_with("LPT"))
                && matches!(base.as_bytes()[3], b'1'..=b'9'));
            if name_text.ends_with(['.', ' ']) || device {
                return Err(reject(
                    "ambiguous Win32 paths and device names are forbidden",
                ));
            }
        }
        if name_text.eq_ignore_ascii_case(".git") || name_text.contains(':') {
            return Err(reject("Git internals and alternate streams are forbidden"));
        }
        result.push(name);
        match fs::symlink_metadata(&result) {
            Ok(meta) if symlink(&meta) => {
                return Err((
                    "unsupported",
                    "symbolic links and reparse points are unsupported".into(),
                ))
            }
            Ok(_) if missing.is_empty() => existing = result.clone(),
            Ok(_) => missing.push(name.to_owned()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => missing.push(name.to_owned()),
            Err(e) => return Err(("unsupported", e.to_string())),
        }
    }
    #[cfg(windows)]
    {
        on_disk_case(root, &existing, &missing)
    }
    #[cfg(not(windows))]
    {
        let _ = (existing, missing);
        Ok(result)
    }
}

/// Windows names are case-insensitive but case-preserving, and a rename onto
/// an existing `README.md` spelled `readme.md` renames the file. Existing
/// components therefore keep their on-disk spelling (links were rejected
/// above, so canonicalizing follows none); new ones keep the history's.
#[cfg(windows)]
fn on_disk_case(
    root: &Path,
    existing: &Path,
    missing: &[std::ffi::OsString],
) -> Result<PathBuf, (&'static str, String)> {
    let mut path = if existing == root {
        root.to_owned()
    } else {
        fs::canonicalize(existing).map_err(|e| ("unsupported", e.to_string()))?
    };
    let prefix = format!("{}/", path_key(&root.to_string_lossy()).trim_end_matches('/'));
    if existing != root && !path_key(&path.to_string_lossy()).starts_with(&prefix) {
        return Err(("outsideWorkspace", "path left the workspace".into()));
    }
    for name in missing {
        path.push(name);
    }
    Ok(path)
}

pub fn read(path: &Path) -> Result<Option<Vec<u8>>, String> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    if symlink(&meta) || !meta.is_file() {
        return Err("not a regular file".into());
    }
    if meta.len() > MAX_BYTES {
        return Err("file exceeds 8 MiB".into());
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|e| e.to_string())?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("file exceeds 8 MiB".into());
    }
    std::str::from_utf8(&bytes).map_err(|_| "file is not UTF-8")?;
    Ok(Some(bytes))
}

fn mode(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).ok().map(|m| m.permissions().mode())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

pub fn build(root: &Path, changes: &[Change], selected: Option<&[String]>) -> Result<Plan, String> {
    build_at(root, root, changes, selected)
}

/// Relative native paths are relative to cwd even when an enclosing project
/// root supplies the wider allowed boundary.
pub fn build_at(
    root: &Path,
    cwd: &Path,
    changes: &[Change],
    selected: Option<&[String]>,
) -> Result<Plan, String> {
    build_at_with_extra(root, cwd, None, changes, selected)
}

pub fn build_at_with_extra(
    root: &Path,
    cwd: &Path,
    extra: Option<&Path>,
    changes: &[Change],
    selected: Option<&[String]>,
) -> Result<Plan, String> {
    // Canonical paths carry the on-disk case on Windows; keys compare by path_key.
    let root = fs::canonicalize(root).map_err(|e| format!("workspace unavailable: {e}"))?;
    if !root.is_dir() {
        return Err("workspace is not a directory".into());
    }
    let extra_root = extra
        .map(|path| {
            let path = fs::canonicalize(path).map_err(|e| e.to_string())?;
            if !path.is_dir() {
                return Err("project workspace is not a directory".to_owned());
            }
            Ok(path)
        })
        .transpose()?;
    let cwd = fs::canonicalize(cwd).map_err(|e| e.to_string())?;
    let relative_cwd = cwd
        .strip_prefix(&root)
        .map_err(|_| "cwd is outside workspace")?;
    let resolve_change = |raw: &str| {
        if Path::new(&raw.replace('\\', "/")).is_absolute() {
            resolve(&root, raw).or_else(|error| {
                if error.0 == "outsideWorkspace" {
                    if let Some(extra) = &extra_root {
                        return resolve(extra, raw);
                    }
                }
                Err(error)
            })
        } else {
            resolve(
                &root,
                &relative_cwd.join(raw.replace('\\', "/")).to_string_lossy(),
            )
        }
    };
    let mut groups: Vec<(BTreeSet<String>, Vec<&Change>)> = Vec::new();
    for change in changes {
        let canonical_key = |p: &str| {
            resolve_change(p)
                .map(|p| path_key(&p.to_string_lossy()))
                .unwrap_or_else(|_| path_key(p))
        };
        let mut keys = BTreeSet::from([canonical_key(&change.path)]);
        if let Some(path) = &change.move_path {
            keys.insert(canonical_key(path));
        }
        let mut list = Vec::new();
        let mut i = 0;
        while i < groups.len() {
            if !groups[i].0.is_disjoint(&keys) {
                let (found, mut prior) = groups.remove(i);
                keys.extend(found);
                list.append(&mut prior);
            } else {
                i += 1;
            }
        }
        list.push(change);
        // Restore source order after merging groups linked by a rename.
        list.sort_by_key(|c| {
            changes
                .iter()
                .position(|candidate| std::ptr::eq(candidate, *c))
                .unwrap()
        });
        groups.push((keys, list));
    }
    if let Some(paths) = selected {
        if paths.iter().any(|p| {
            !changes.iter().any(|c| {
                path_key(&c.path) == path_key(p)
                    || c.move_path
                        .as_ref()
                        .is_some_and(|m| path_key(m) == path_key(p))
            })
        }) {
            return Err("paths must use an original change path".into());
        }
    }
    let selected = selected.map(|paths| {
        paths
            .iter()
            .map(|p| {
                resolve_change(p)
                    .map(|p| path_key(&p.to_string_lossy()))
                    .unwrap_or_else(|_| path_key(p))
            })
            .collect::<BTreeSet<_>>()
    });
    if let Some(paths) = &selected {
        if paths.is_empty()
            || paths
                .iter()
                .any(|p| !groups.iter().any(|(keys, _)| keys.contains(p)))
        {
            return Err("paths must name changes in this turn".into());
        }
    }
    let mut files = Vec::new();
    let mut writes = Vec::new();
    let mut digests = Vec::new();
    for (keys, list) in groups {
        if selected
            .as_ref()
            .is_some_and(|selected| keys.is_disjoint(selected))
        {
            continue;
        }
        let first = list[0];
        let move_path = list.iter().rev().find_map(|c| c.move_path.clone());
        let action = if move_path.is_some() {
            "moveBack"
        } else {
            match first.kind.as_str() {
                "add" => "delete",
                "delete" => "recreate",
                _ => "restore",
            }
        };
        let mut file = FilePlan {
            path: first.path.clone(),
            relative_path: None,
            move_path,
            action: action.into(),
            status: "alreadyReverted".into(),
            reason: None,
            added: 0,
            deleted: 0,
        };
        let mut virtual_fs = BTreeMap::new();
        let mut original = BTreeMap::new();
        let mut resolved = BTreeMap::new();
        for raw in list
            .iter()
            .flat_map(|c| std::iter::once(&c.path).chain(c.move_path.iter()))
        {
            match resolve_change(raw).and_then(|path| {
                if path
                    .extension()
                    .is_some_and(|s| s.eq_ignore_ascii_case("ipynb"))
                {
                    return Err(("unsupported", "notebook diffs are approximate".into()));
                }
                let bytes = read(&path).map_err(|e| ("unsupported", e))?;
                Ok((path, bytes))
            }) {
                Ok((path, bytes)) => {
                    // One virtual file per path_key (case-insensitive on Windows),
                    // even when the history spells a not-yet-existing path two ways.
                    let path = original
                        .keys()
                        .find(|known: &&PathBuf| {
                            path_key(&known.to_string_lossy()) == path_key(&path.to_string_lossy())
                        })
                        .cloned()
                        .unwrap_or(path);
                    if raw == &file.path {
                        file.relative_path = path
                            .strip_prefix(&root)
                            .ok()
                            .or_else(|| {
                                extra_root
                                    .as_ref()
                                    .and_then(|root| path.strip_prefix(root).ok())
                            })
                            .map(|p| p.to_string_lossy().replace('\\', "/"));
                    }
                    original.insert(path.clone(), bytes.clone());
                    virtual_fs.insert(path.clone(), bytes);
                    resolved.insert(path_key(raw), path);
                }
                Err((status, reason)) => {
                    file.status = status.into();
                    file.reason = Some(reason);
                }
            }
        }
        if file.reason.is_none() {
            for change in list.iter().rev() {
                if change.diff.len() as u64 > MAX_BYTES {
                    file.status = "unsupported".into();
                    file.reason = Some("diff exceeds 8 MiB".into());
                    break;
                }
                let path = &resolved[&path_key(&change.path)];
                let source = change
                    .move_path
                    .as_ref()
                    .map(|p| &resolved[&path_key(p)])
                    .unwrap_or(path);
                let current = virtual_fs[source].clone();
                let outcome: Result<Option<Vec<u8>>, (&str, String)> =
                    (|| match change.kind.as_str() {
                        "add" => match current.as_deref() {
                            None => Ok(None),
                            Some(bytes)
                                if patch::equivalent(
                                    std::str::from_utf8(bytes).unwrap(),
                                    &change.diff,
                                ) =>
                            {
                                Ok(None)
                            }
                            _ => Err(("conflict", "added file has changed".into())),
                        },
                        "delete" => match current.as_deref() {
                            None => Ok(Some(change.diff.as_bytes().to_vec())),
                            Some(bytes)
                                if patch::equivalent(
                                    std::str::from_utf8(bytes).unwrap(),
                                    &change.diff,
                                ) =>
                            {
                                Ok(current.clone())
                            }
                            _ => Err((
                                "conflict",
                                "deleted path now contains different content".into(),
                            )),
                        },
                        "update" => {
                            if change.move_path.is_some() && virtual_fs[path].is_some() {
                                if current.is_none()
                                    && patch::reverse(
                                        std::str::from_utf8(virtual_fs[path].as_ref().unwrap())
                                            .unwrap(),
                                        &change.diff,
                                    )
                                    .is_ok_and(|r| r.already)
                                {
                                    return Ok(virtual_fs[path].clone());
                                }
                                return Err(("conflict", "move destination already exists".into()));
                            }
                            let bytes = current
                                .as_ref()
                                .ok_or(("missing", "file to restore is missing".into()))?;
                            let reversed =
                                patch::reverse(std::str::from_utf8(bytes).unwrap(), &change.diff)
                                    .map_err(|e| ("conflict", e))?;
                            file.added += reversed.added;
                            file.deleted += reversed.deleted;
                            Ok(Some(reversed.text.into_bytes()))
                        }
                        _ => Err(("unsupported", "unsupported change kind".into())),
                    })();
                match outcome {
                    Ok(after) => {
                        if after
                            .as_ref()
                            .is_some_and(|bytes| bytes.len() as u64 > MAX_BYTES)
                        {
                            file.status = "unsupported".into();
                            file.reason = Some("restored file exceeds 8 MiB".into());
                            break;
                        }
                        if change.kind == "add" && current.is_some() {
                            file.deleted += change.diff.lines().count();
                        }
                        if change.kind == "delete" && current.is_none() {
                            file.added += change.diff.lines().count();
                        }
                        if virtual_fs[path] != after
                            || (source != path && virtual_fs[source].is_some())
                        {
                            file.status = "ok".into();
                        }
                        virtual_fs.insert(path.clone(), after);
                        if source != path {
                            virtual_fs.insert(source.clone(), None);
                        }
                    }
                    Err((status, reason)) => {
                        file.status = status.into();
                        file.reason = Some(reason);
                        break;
                    }
                }
            }
        }
        // When the same file was changed several times or moved through a
        // chain, the final patch's old side is not the original file. Prove
        // already-reverted by replaying the entire chain in memory only.
        if matches!(file.status.as_str(), "missing" | "conflict")
            && prove_before(&list, &resolved, &original)
        {
            file.status = "alreadyReverted".into();
            file.reason = None;
            file.added = 0;
            file.deleted = 0;
            virtual_fs = original.clone();
        }
        let states: Vec<_> = original
            .iter()
            .map(|(path, before)| {
                json!([path, content_hash(before), content_hash(&virtual_fs[path])])
            })
            .collect();
        digests.push(json!([
            file.path,
            file.move_path,
            file.action,
            file.status,
            states
        ]));
        if file.status == "ok" {
            let moved_mode = if file.move_path.is_some() {
                original.keys().find_map(|path| mode(path))
            } else {
                None
            };
            for (path, before) in original {
                let after = virtual_fs.remove(&path).unwrap();
                if before != after {
                    writes.push(Write {
                        mode: mode(&path).or(moved_mode),
                        path,
                        before,
                        after,
                    });
                }
            }
        }
        files.push(file);
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    digests.sort_by_key(Value::to_string);
    writes.sort_by(|a, b| a.path.cmp(&b.path));
    let digest = hash(&serde_json::to_vec(&json!([root, extra_root, digests])).unwrap());
    Ok(Plan {
        root,
        extra_root,
        files,
        writes,
        digest,
    })
}

fn prove_before(
    changes: &[&Change],
    resolved: &BTreeMap<String, PathBuf>,
    before: &BTreeMap<PathBuf, Option<Vec<u8>>>,
) -> bool {
    let mut vfs = before.clone();
    for change in changes {
        let Some(path) = resolved.get(&path_key(&change.path)) else {
            return false;
        };
        let target = change
            .move_path
            .as_ref()
            .and_then(|p| resolved.get(&path_key(p)))
            .unwrap_or(path);
        let Some(bytes) = vfs.get(path) else {
            return false;
        };
        let after = match change.kind.as_str() {
            "add" if bytes.is_none() => Some(change.diff.as_bytes().to_vec()),
            "delete"
                if bytes.as_ref().is_some_and(|b| {
                    patch::equivalent(std::str::from_utf8(b).unwrap(), &change.diff)
                }) =>
            {
                None
            }
            "update" => {
                if target != path && vfs.get(target).is_some_and(Option::is_some) {
                    return false;
                }
                let Some(bytes) = bytes else {
                    return false;
                };
                let Ok(next) = patch::forward(std::str::from_utf8(bytes).unwrap(), &change.diff)
                else {
                    return false;
                };
                if next.already && target == path {
                    return false;
                }
                Some(next.text.into_bytes())
            }
            _ => return false,
        };
        vfs.insert(target.clone(), after);
        if target != path {
            vfs.insert(path.clone(), None);
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    fn root() -> PathBuf {
        let p = std::env::temp_dir().join(format!("cutex-files-plan-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&p).unwrap();
        p
    }
    fn change(path: &str, kind: &str, diff: &str) -> Change {
        Change {
            path: path.into(),
            kind: kind.into(),
            diff: diff.into(),
            move_path: None,
        }
    }
    #[test]
    fn repeated_updates_and_rename_chain_are_reversed_in_order() {
        let root = root();
        fs::write(root.join("c"), "three\n").unwrap();
        let mut a = change("a", "update", "-one\n+two\n");
        a.move_path = Some("b".into());
        let mut b = change("b", "update", "-two\n+three\n");
        b.move_path = Some("c".into());
        let changes = [a, b];
        let plan = build(&root, &changes, Some(&["b".into()])).unwrap();
        assert!(plan.applicable());
        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.writes.len(), 2);
        assert_eq!(plan.writes[0].after, Some(b"one\n".to_vec()));
        fs::remove_file(root.join("c")).unwrap();
        fs::write(root.join("a"), "one\n").unwrap();
        assert_eq!(
            build(&root, &changes, None).unwrap().files[0].status,
            "alreadyReverted"
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn statuses_selection_and_path_boundaries() {
        let root = root();
        fs::write(root.join("added"), "changed").unwrap();
        fs::write(root.join("binary"), [255]).unwrap();
        let changes = [
            change("added", "add", "original"),
            change("missing", "update", "-a\n+b"),
            change(".git/config", "delete", "x"),
            change("../escape", "add", "x"),
            change("binary", "delete", "x"),
            change("a.ipynb", "delete", "x"),
            change("deleted", "delete", "old"),
            change("gone", "add", "new"),
        ];
        let plan = build(&root, &changes, None).unwrap();
        for status in [
            "conflict",
            "missing",
            "outsideWorkspace",
            "unsupported",
            "ok",
            "alreadyReverted",
        ] {
            assert!(plan.files.iter().any(|f| f.status == status), "{status}");
        }
        assert!(!plan.applicable());
        assert!(build(&root, &changes, Some(&["deleted".into()]))
            .unwrap()
            .applicable());
        assert!(build(&root, &changes, Some(&["unknown".into()])).is_err());
        #[cfg(windows)]
        for alias in [".git./config", "NUL", "normal. ", "CON.txt"] {
            assert_eq!(
                build(&root, &[change(alias, "delete", "content")], None)
                    .unwrap()
                    .files[0]
                    .status,
                "outsideWorkspace"
            );
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn completed_only_and_warnings() {
        let (changes, warnings) = changes(&json!({"items":[{"type":"fileChange","status":"failed"},{"type":"fileChange","status":"declined"},{"type":"commandExecution"},{"type":"collabAgentToolCall"}]})).unwrap();
        assert!(changes.is_empty());
        assert_eq!(warnings.len(), 3);
        assert_eq!(warnings[2]["count"], 2);
    }
    #[test]
    fn repeated_file_changes_absolute_aliases_and_cwd_under_project() {
        let root = root();
        let cwd = root.join("src");
        fs::create_dir(&cwd).unwrap();
        fs::write(cwd.join("a"), "three\r\n").unwrap();
        let first = change("a", "update", "-one\n+two\n");
        let second = change(&cwd.join("a").to_string_lossy(), "update", "-two\n+three\n");
        let changes = [first, second];
        let plan = build_at(&root, &cwd, &changes, None).unwrap();
        assert!(plan.applicable());
        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.writes[0].after, Some(b"one\r\n".to_vec()));
        fs::write(cwd.join("a"), "one\r\n").unwrap();
        assert_eq!(
            build_at(&root, &cwd, &changes, None).unwrap().files[0].status,
            "alreadyReverted"
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn unchanged_move_and_oversize_files() {
        let root = root();
        fs::write(root.join("b"), "same").unwrap();
        let mut moved = change("a", "update", "");
        moved.move_path = Some("b".into());
        assert!(build(&root, &[moved], None).unwrap().applicable());
        fs::File::create(root.join("large"))
            .unwrap()
            .set_len(MAX_BYTES + 1)
            .unwrap();
        assert_eq!(
            build(&root, &[change("large", "delete", "")], None)
                .unwrap()
                .files[0]
                .status,
            "unsupported"
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn disjoint_project_root_is_allowed_only_when_explicitly_supplied() {
        let root = root();
        let project = root.join("project");
        let cwd = root.join("cwd");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&cwd).unwrap();
        fs::write(project.join("a"), "new\n").unwrap();
        let changes = [change(
            &project.join("a").to_string_lossy(),
            "update",
            "-old\n+new\n",
        )];
        assert_eq!(
            build(&cwd, &changes, None).unwrap().files[0].status,
            "outsideWorkspace"
        );
        let plan = build_at_with_extra(&cwd, &cwd, Some(&project), &changes, None).unwrap();
        assert!(plan.applicable());
        assert_eq!(plan.files[0].relative_path.as_deref(), Some("a"));
        let home = root.join("journal");
        fs::create_dir(&home).unwrap();
        super::super::journal::execute(&home, &plan, &plan.digest).unwrap();
        assert_eq!(fs::read_to_string(project.join("a")).unwrap(), "old\n");
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn symlinks_inside_and_outside_workspace_are_never_followed() {
        let root = root();
        let target = root.join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("a"), "new").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, root.join("link")).unwrap();
        #[cfg(windows)]
        {
            // Junctions exercise ancestor reparse rejection without requiring
            // the Windows CreateSymbolicLink privilege. Fixture creation only.
            use std::os::windows::process::CommandExt;
            let output=std::process::Command::new("powershell.exe").creation_flags(0x08000000).args(["-NoProfile","-NonInteractive","-Command","New-Item -ItemType Junction -Path $env:CUTEX_FRV_LINK -Target $env:CUTEX_FRV_TARGET | Out-Null"]).env("CUTEX_FRV_LINK",root.join("link")).env("CUTEX_FRV_TARGET",&target).output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert_eq!(
            build(&root, &[change("link/a", "update", "-old\n+new")], None)
                .unwrap()
                .files[0]
                .status,
            "unsupported"
        );
        assert_eq!(fs::read_to_string(target.join("a")).unwrap(), "new");
        fs::remove_dir_all(root).unwrap();
    }
}
