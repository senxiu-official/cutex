//! Owner-only `GET|POST /v2/claude/skills/import` (capability
//! `claude-skills-import`): copy Claude Code skills (by default the host
//! user's `~/.claude/skills`) into the host's shared Claude skills root
//! `~/.cutex/claude-skills` (`runtime::claude::structure::user_skills_root`),
//! which every Claude agent of the host loads. Claude Code's own directory is
//! never read by agents (and a link to it would be refused by the managed
//! root containment), so importing means copying.
//!
//! GET `[?source=<absolute dir>]` -> `{source, defaultSource, sourceExists,
//! target, skills: [{name, title, description, bytes, files, exists,
//! importable, reason?, skippedLinks}], truncated}`. A skill is an immediate
//! child directory of the source with a `SKILL.md`.
//!
//! POST `{source?, names: [..], overwrite?}` -> `{imported: [name],
//! skipped: [{name, reason}], failed: [{name, reason, message}], target}`.
//! An existing skill of the same name is skipped (`exists`) unless
//! `overwrite`; it is then replaced as a whole.
//!
//! Rules: the source is only read. Symlinks are never followed out of the
//! source (a skill directory that is such a link is not importable, such a
//! link inside a skill is left out and counted in `skippedLinks`); links to
//! directories are never followed. Each skill is bounded (`MAX_SKILL_BYTES`,
//! `MAX_SKILL_FILES`, `MAX_DEPTH`, `SKILL.md` at most 48 KiB as the catalog
//! reads it). The copy is staged inside the target root and renamed into
//! place.

use std::collections::HashSet;
use std::net::TcpStream;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::http::server::{write_json_response, SimpleHttpRequest};

pub const CAPABILITY: &str = "claude-skills-import";
pub const PATH: &str = "/v2/claude/skills/import";

const MAX_SKILL_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SKILL_FILES: usize = 2000;
const MAX_DEPTH: usize = 16;
const MAX_SKILL_MD_BYTES: u64 = 48 * 1024;
const MAX_LISTED: usize = 256;
const MAX_SCANNED: usize = 1024;
const MAX_NAMES: usize = 64;
const STAGING_PREFIX: &str = ".import-";

pub fn is_path(path: &str) -> bool {
    path.split('?').next() == Some(PATH)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ImportBody {
    #[serde(default)]
    source: Option<String>,
    names: Vec<String>,
    #[serde(default)]
    overwrite: bool,
}

fn error(stream: &mut TcpStream, status: u16, code: &str, message: &str) -> anyhow::Result<()> {
    let reason = super::server::http_reason_for(status);
    super::server::write_v2_error(stream, status, reason, code, message, status == 503, json!({}))
}

/// The host user's Claude Code skills directory (`~/.claude/skills`).
pub fn default_source() -> Option<PathBuf> {
    crate::config::paths::home_dir().map(|home| home.join(".claude").join("skills"))
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let target = match crate::runtime::claude::structure::user_skills_root() {
        Ok(target) => target,
        Err(err) => return error(stream, 503, "cutex_home_unavailable", &format!("{err:#}")),
    };
    match request.method.as_str() {
        "GET" => {
            let source = crate::http::query::query_value(&request.path, "source")
                .map(|raw| super::attachments::percent_decode(&raw.replace('+', " ")).unwrap_or_default());
            let source = match resolve_source(source.as_deref()) {
                Ok(source) => source,
                Err(message) => return error(stream, 400, "invalid_source", &message),
            };
            match list(&source, &target) {
                Ok(body) => write_json_response(stream, 200, "OK", &body),
                Err(ImportError::Invalid(message)) => error(stream, 400, "invalid_source", &message),
                Err(ImportError::Io(err)) => error(stream, 409, "source_unreadable", &format!("{err:#}")),
            }
        }
        "POST" => {
            let body: ImportBody = match serde_json::from_slice(&request.body) {
                Ok(body) => body,
                Err(err) => {
                    return error(stream, 400, "invalid_request", &format!("body must be {{source?, names, overwrite?}}: {err}"))
                }
            };
            if body.names.is_empty() || body.names.len() > MAX_NAMES {
                return error(stream, 400, "invalid_request", &format!("names must list 1 to {MAX_NAMES} skills"));
            }
            let source = match resolve_source(body.source.as_deref()) {
                Ok(source) => source,
                Err(message) => return error(stream, 400, "invalid_source", &message),
            };
            match import(&source, &target, &body.names, body.overwrite) {
                Ok(body) => write_json_response(stream, 200, "OK", &body),
                Err(ImportError::Invalid(message)) => error(stream, 400, "invalid_source", &message),
                Err(ImportError::Io(err)) => error(stream, 409, "import_failed", &format!("{err:#}")),
            }
        }
        _ => error(stream, 405, "method_not_allowed", "use GET or POST for /v2/claude/skills/import"),
    }
}

#[derive(Debug)]
pub enum ImportError {
    Invalid(String),
    Io(anyhow::Error),
}

impl From<anyhow::Error> for ImportError {
    fn from(err: anyhow::Error) -> Self {
        Self::Io(err)
    }
}

fn resolve_source(raw: Option<&str>) -> Result<PathBuf, String> {
    match raw.filter(|raw| !raw.is_empty()) {
        Some(raw) => {
            let path = PathBuf::from(raw);
            if !path.is_absolute() || raw.chars().any(char::is_control) || raw.chars().count() > 4096 {
                return Err("source must be an absolute directory path".into());
            }
            Ok(path)
        }
        None => default_source().ok_or_else(|| "the host user's home directory is unknown; pass source".to_string()),
    }
}

fn public(path: &Path) -> String {
    crate::agent_management::strip_verbatim_prefix(&path.to_string_lossy())
}

/// A valid skill directory name: one plain path component.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= 128
        && !name.starts_with('.')
        && !name.contains(['/', '\\', ':'])
        && !name.chars().any(char::is_control)
        && name.trim() == name
}

/// Canonical source root and target root, refusing overlapping directories.
/// `None` for the source = it does not exist.
fn roots(source: &Path, target: &Path) -> Result<(Option<PathBuf>, PathBuf), ImportError> {
    let source = match source.canonicalize() {
        Ok(path) if path.is_dir() => Some(path),
        Ok(_) => return Err(ImportError::Invalid("source is not a directory".into())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(ImportError::Io(anyhow::Error::new(err).context("source unreadable"))),
    };
    // The target may not exist yet: compare against its nearest existing ancestor.
    let target_abs = match target.canonicalize() {
        Ok(path) => path,
        Err(_) => {
            let parent = target.parent().and_then(|parent| parent.canonicalize().ok());
            match (parent, target.file_name()) {
                (Some(parent), Some(name)) => parent.join(name),
                _ => target.to_path_buf(),
            }
        }
    };
    if let Some(source) = &source {
        if source.starts_with(&target_abs) || target_abs.starts_with(source) {
            return Err(ImportError::Invalid("source and the shared Claude skills directory overlap".into()));
        }
    }
    Ok((source, target_abs))
}

#[derive(Debug, Default)]
struct Scan {
    bytes: u64,
    files: usize,
    skipped_links: usize,
    /// (relative path, file to read) in copy order; directories are created on the way.
    entries: Vec<(PathBuf, PathBuf)>,
    dirs: Vec<PathBuf>,
}

/// Why a skill cannot be imported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    SymlinkOutsideSource,
    TooLarge,
    TooManyFiles,
    TooDeep,
    SkillFileTooLarge,
    InvalidName,
    NotFound,
}

impl Refusal {
    fn as_str(self) -> &'static str {
        match self {
            Self::SymlinkOutsideSource => "symlinkOutsideSource",
            Self::TooLarge => "tooLarge",
            Self::TooManyFiles => "tooManyFiles",
            Self::TooDeep => "tooDeep",
            Self::SkillFileTooLarge => "skillFileTooLarge",
            Self::InvalidName => "invalidName",
            Self::NotFound => "notFound",
        }
    }
}

/// The real directory of skill `name` inside `source` (canonical), or why
/// not; `Ok(None)` = not a skill (no directory or no `SKILL.md`).
fn skill_dir(source: &Path, name: &str) -> Result<Option<PathBuf>, Refusal> {
    if !valid_name(name) {
        return Err(Refusal::InvalidName);
    }
    let entry = source.join(name);
    let Ok(meta) = std::fs::symlink_metadata(&entry) else {
        return Ok(None);
    };
    let dir = if meta.file_type().is_symlink() {
        let Ok(resolved) = entry.canonicalize() else { return Ok(None) };
        if !resolved.starts_with(source) || resolved == source {
            return Err(Refusal::SymlinkOutsideSource);
        }
        resolved
    } else {
        entry
    };
    if !dir.is_dir() {
        return Ok(None);
    }
    let skill_md = dir.join("SKILL.md");
    match std::fs::symlink_metadata(&skill_md) {
        Ok(meta) if meta.is_file() => {
            if meta.len() > MAX_SKILL_MD_BYTES {
                return Err(Refusal::SkillFileTooLarge);
            }
            Ok(Some(dir))
        }
        Ok(meta) if meta.file_type().is_symlink() => match skill_md.canonicalize() {
            Ok(real) if real.starts_with(source) && real.is_file() => {
                if std::fs::metadata(&real).map(|m| m.len()).unwrap_or(0) > MAX_SKILL_MD_BYTES {
                    return Err(Refusal::SkillFileTooLarge);
                }
                Ok(Some(dir))
            }
            Ok(_) => Err(Refusal::SymlinkOutsideSource),
            Err(_) => Ok(None),
        },
        _ => Ok(None),
    }
}

/// Walks a skill directory without leaving the source: regular files are
/// copied, links to files inside the source are copied as files, every other
/// link is left out.
fn scan(source: &Path, dir: &Path) -> Result<Scan, Refusal> {
    let mut scan = Scan::default();
    let mut stack = vec![(dir.to_path_buf(), PathBuf::new(), 0usize)];
    while let Some((current, relative, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            return Err(Refusal::TooDeep);
        }
        let Ok(entries) = std::fs::read_dir(&current) else { continue };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let rel = relative.join(entry.file_name());
            let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
            let file = if meta.file_type().is_symlink() {
                match path.canonicalize() {
                    Ok(real) if real.starts_with(source) && real.is_file() => real,
                    _ => {
                        scan.skipped_links += 1;
                        continue;
                    }
                }
            } else if meta.is_dir() {
                scan.dirs.push(rel.clone());
                stack.push((path, rel, depth + 1));
                continue;
            } else if meta.is_file() {
                path
            } else {
                scan.skipped_links += 1;
                continue;
            };
            let len = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
            scan.files += 1;
            scan.bytes = scan.bytes.saturating_add(len);
            if scan.files > MAX_SKILL_FILES {
                return Err(Refusal::TooManyFiles);
            }
            if scan.bytes > MAX_SKILL_BYTES {
                return Err(Refusal::TooLarge);
            }
            scan.entries.push((rel, file));
        }
    }
    Ok(scan)
}

/// `name` / `description` from `SKILL.md` front matter (plain `key: value`
/// lines, no YAML evaluation).
fn front_matter(skill_md: &Path) -> (Option<String>, Option<String>) {
    let Ok(raw) = std::fs::read(skill_md) else { return (None, None) };
    let text = String::from_utf8_lossy(&raw[..raw.len().min(MAX_SKILL_MD_BYTES as usize)]).into_owned();
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (None, None);
    }
    let (mut name, mut description) = (None, None);
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'').trim();
        if value.is_empty() {
            continue;
        }
        let value: String = value.chars().take(512).collect();
        match key.trim() {
            "name" => name = Some(value),
            "description" => description = Some(value),
            _ => {}
        }
    }
    (name, description)
}

/// The importable skills of `source` (GET answer).
pub fn list(source: &Path, target: &Path) -> Result<Value, ImportError> {
    let (real, target_abs) = roots(source, target)?;
    let mut skills = Vec::new();
    let mut truncated = false;
    if let Some(real) = &real {
        let mut names: Vec<String> = std::fs::read_dir(real)
            .context("source unreadable")?
            .take(MAX_SCANNED)
            .flatten()
            .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
            .filter(|name| !name.starts_with('.'))
            .collect();
        names.sort();
        for name in names {
            let dir = match skill_dir(real, &name) {
                Ok(Some(dir)) => dir,
                Ok(None) => continue,
                Err(Refusal::InvalidName) => continue,
                Err(refusal) => {
                    skills.push(json!({"name": name, "title": null, "description": null, "bytes": null, "files": null,
                        "exists": exists(&target_abs, &name), "importable": false, "reason": refusal.as_str(), "skippedLinks": 0}));
                    continue;
                }
            };
            if skills.len() >= MAX_LISTED {
                truncated = true;
                break;
            }
            let (title, description) = front_matter(&dir.join("SKILL.md"));
            let mut item = json!({"name": name, "title": title, "description": description,
                "exists": exists(&target_abs, &name)});
            match scan(real, &dir) {
                Ok(found) => {
                    item["bytes"] = json!(found.bytes);
                    item["files"] = json!(found.files);
                    item["importable"] = json!(true);
                    item["skippedLinks"] = json!(found.skipped_links);
                }
                Err(refusal) => {
                    item["bytes"] = Value::Null;
                    item["files"] = Value::Null;
                    item["importable"] = json!(false);
                    item["reason"] = json!(refusal.as_str());
                    item["skippedLinks"] = json!(0);
                }
            }
            skills.push(item);
        }
    }
    let default_source = default_source().map(|path| public(&path));
    Ok(json!({
        "source": public(source),
        "defaultSource": default_source,
        "sourceExists": real.is_some(),
        "target": public(&target_abs),
        "skills": skills,
        "truncated": truncated,
    }))
}

fn exists(target: &Path, name: &str) -> bool {
    std::fs::symlink_metadata(target.join(name)).is_ok()
}

/// Copies the named skills (POST answer).
pub fn import(source: &Path, target: &Path, names: &[String], overwrite: bool) -> Result<Value, ImportError> {
    let (real, target_abs) = roots(source, target)?;
    let Some(real) = real else {
        return Err(ImportError::Invalid("source does not exist".into()));
    };
    let mut seen = HashSet::new();
    let (mut imported, mut skipped, mut failed) = (Vec::new(), Vec::new(), Vec::new());
    let mut prepared = false;
    for name in names {
        if !seen.insert(name.clone()) {
            continue;
        }
        let dir = match skill_dir(&real, name) {
            Ok(Some(dir)) => dir,
            Ok(None) => {
                failed.push(json!({"name": name, "reason": Refusal::NotFound.as_str(), "message": "no skill of that name in the source"}));
                continue;
            }
            Err(refusal) => {
                failed.push(json!({"name": name, "reason": refusal.as_str(), "message": refusal.as_str()}));
                continue;
            }
        };
        if exists(&target_abs, name) && !overwrite {
            skipped.push(json!({"name": name, "reason": "exists"}));
            continue;
        }
        let found = match scan(&real, &dir) {
            Ok(found) => found,
            Err(refusal) => {
                failed.push(json!({"name": name, "reason": refusal.as_str(), "message": refusal.as_str()}));
                continue;
            }
        };
        if !prepared {
            crate::profiles::materialize::create_private_dir_all(&target_abs)?;
            prepared = true;
        }
        match copy_into(&target_abs, name, &found) {
            Ok(()) => imported.push(json!(name)),
            Err(err) => failed.push(json!({"name": name, "reason": "copyFailed", "message": format!("{err:#}")})),
        }
    }
    Ok(json!({"imported": imported, "skipped": skipped, "failed": failed, "target": public(&target_abs)}))
}

/// Stages the copy in `target/.import-<id>/<name>`, then swaps it in.
fn copy_into(target: &Path, name: &str, found: &Scan) -> anyhow::Result<()> {
    let staging_root = target.join(format!("{STAGING_PREFIX}{}", uuid::Uuid::new_v4().simple()));
    let staged = staging_root.join(name);
    let result = (|| -> anyhow::Result<()> {
        std::fs::create_dir_all(&staged).with_context(|| format!("create {}", staged.display()))?;
        for dir in &found.dirs {
            std::fs::create_dir_all(staged.join(dir))?;
        }
        for (relative, file) in &found.entries {
            let to = staged.join(relative);
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(file, &to).with_context(|| format!("copy {}", relative.display()))?;
        }
        let dest = target.join(name);
        if std::fs::symlink_metadata(&dest).is_ok() {
            let old = staging_root.join(format!("{name}.old"));
            std::fs::rename(&dest, &old).with_context(|| format!("move the existing {name} aside"))?;
            if let Err(err) = std::fs::rename(&staged, &dest) {
                let _ = std::fs::rename(&old, &dest);
                return Err(anyhow::Error::new(err).context(format!("replace {name}")));
            }
        } else {
            std::fs::rename(&staged, &dest).with_context(|| format!("place {name}"))?;
        }
        Ok(())
    })();
    // The staging root holds only the copy (or the replaced skill): remove it.
    let _ = remove_tree(&staging_root);
    result
}

/// Removes a directory tree without following links (only inside our staging root).
fn remove_tree(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
#[path = "claude_skills_import_tests.rs"]
mod tests;
