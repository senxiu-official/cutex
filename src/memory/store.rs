//! File store for one project's memory directory.
//!
//! Layout: `<dir>/<file>.md` (one fact per file, YAML frontmatter) plus the
//! `<dir>/MEMORY.md` index. File names are restricted to
//! `[a-z0-9][a-z0-9._-]*\.md` (or exactly `MEMORY.md`), so a request can never
//! name a path outside the directory. Revisions are content hashes
//! (`sha256:<hex>`) used for optimistic concurrency by the Management API.

use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

use crate::agent_management::ProjectId;

pub const MEMORY_INDEX_FILE: &str = "MEMORY.md";
pub const MEMORY_TYPES: [&str; 4] = ["user", "feedback", "project", "reference"];
pub const MAX_MEMORY_FILE_NAME_CHARS: usize = 128;
pub const MAX_MEMORY_FILE_BYTES: usize = 64 * 1024;
pub const MAX_MEMORY_INDEX_BYTES: usize = 256 * 1024;
pub const MAX_MEMORY_NAME_CHARS: usize = 64;
pub const MAX_MEMORY_DESCRIPTION_CHARS: usize = 300;

/// `~/.cutex/projects` (honours the debug private-home boundary).
pub fn projects_root() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::config_dir()?.join("projects"))
}

/// Filesystem-safe, deterministic directory key for a project id.
///
/// Project ids may contain `:` and differ only by case, neither of which is
/// safe on Windows, so the key is a readable lowercase slug plus a short hash
/// of the exact id.
pub fn project_memory_key(project_id: &str) -> String {
    let mut slug = String::new();
    for ch in project_id.chars() {
        let ch = ch.to_ascii_lowercase();
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
        if slug.len() >= 48 {
            break;
        }
    }
    let slug = slug.trim_end_matches('-');
    let hash = hex(&Sha256::digest(project_id.as_bytes()));
    if slug.is_empty() {
        format!("project-{}", &hash[..12])
    } else {
        format!("{slug}-{}", &hash[..12])
    }
}

/// `~/.cutex/projects/<project-key>/memory`.
pub fn project_memory_dir(project_id: &ProjectId) -> anyhow::Result<PathBuf> {
    Ok(projects_root()?
        .join(project_memory_key(project_id.as_str()))
        .join("memory"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemoryFileName {
    Index,
    Entry(String),
}

impl MemoryFileName {
    pub fn parse(value: &str) -> Result<Self, MemoryError> {
        if value == MEMORY_INDEX_FILE {
            return Ok(Self::Index);
        }
        let valid = value.len() <= MAX_MEMORY_FILE_NAME_CHARS
            && value.len() > ".md".len()
            && value.ends_with(".md")
            && value
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            && value.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-')
            });
        if valid {
            Ok(Self::Entry(value.to_string()))
        } else {
            Err(MemoryError::InvalidFileName)
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Index => MEMORY_INDEX_FILE,
            Self::Entry(name) => name,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct MemoryFrontmatter {
    pub name: Option<String>,
    pub description: Option<String>,
    #[serde(rename = "type")]
    pub memory_type: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryEntry {
    pub file: String,
    pub name: Option<String>,
    pub description: Option<String>,
    #[serde(rename = "type")]
    pub memory_type: Option<String>,
    pub bytes: u64,
    pub updated_at: Option<String>,
    pub revision: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryDocument {
    pub file: String,
    pub content: String,
    /// `None` for `MEMORY.md` and for files without a frontmatter block.
    pub frontmatter: Option<MemoryFrontmatter>,
    pub revision: String,
    pub bytes: u64,
    pub updated_at: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryChange {
    Created,
    Updated,
    Deleted,
}

impl MemoryChange {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Updated => "updated",
            Self::Deleted => "deleted",
        }
    }
}

/// Optimistic concurrency precondition for a mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExpectedRevision {
    /// No precondition.
    Any,
    /// The file must not exist (create-only).
    Absent,
    /// The current content hash must equal this revision.
    Exact(String),
}

#[derive(Debug)]
pub enum MemoryError {
    InvalidFileName,
    InvalidContent(String),
    NotFound,
    RevisionConflict { current: Option<String> },
    Io(anyhow::Error),
}

impl std::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidFileName => write!(
                f,
                "memory file names must match [a-z0-9][a-z0-9._-]*.md (max {MAX_MEMORY_FILE_NAME_CHARS} chars) or be MEMORY.md"
            ),
            Self::InvalidContent(message) => write!(f, "{message}"),
            Self::NotFound => write!(f, "memory file not found"),
            Self::RevisionConflict { .. } => write!(f, "memory file revision does not match expectedRevision"),
            Self::Io(error) => write!(f, "memory store unavailable: {error:#}"),
        }
    }
}

impl std::error::Error for MemoryError {}

impl From<anyhow::Error> for MemoryError {
    fn from(error: anyhow::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryWriteOutcome {
    pub change: MemoryChange,
    pub document: MemoryDocument,
    pub index_updated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryDeleteOutcome {
    pub index_updated: bool,
}

pub fn content_revision(content: &[u8]) -> String {
    format!("sha256:{}", hex(&Sha256::digest(content)))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Parse the leading `---` YAML frontmatter block. Only the keys cutex needs
/// are understood: top-level `name`, `description`, and `metadata.type`
/// (top-level `type` is accepted as a fallback). Returns `None` when the file
/// has no closed frontmatter block.
pub fn parse_frontmatter(content: &str) -> Option<MemoryFrontmatter> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let mut lines = content.lines();
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    let mut frontmatter = MemoryFrontmatter::default();
    let mut top_level_type = None;
    let mut parent: Option<String> = None;
    let mut closed = false;
    for line in lines {
        let trimmed_end = line.trim_end();
        if trimmed_end == "---" || trimmed_end == "..." {
            closed = true;
            break;
        }
        let body = trimmed_end.trim_start();
        if body.is_empty() || body.starts_with('#') {
            continue;
        }
        let indented = body.len() != trimmed_end.len();
        let Some((key, value)) = body.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = unquote_yaml_scalar(value.trim());
        if !indented {
            if value.is_empty() {
                parent = Some(key.to_string());
                continue;
            }
            parent = None;
            match key {
                "name" => frontmatter.name = Some(value),
                "description" => frontmatter.description = Some(value),
                "type" => top_level_type = Some(value),
                _ => {}
            }
        } else if parent.as_deref() == Some("metadata") && key == "type" && !value.is_empty() {
            frontmatter.memory_type = Some(value);
        }
    }
    if !closed {
        return None;
    }
    if frontmatter.memory_type.is_none() {
        frontmatter.memory_type = top_level_type;
    }
    Some(frontmatter)
}

fn unquote_yaml_scalar(value: &str) -> String {
    let value = match value.find(" #") {
        Some(index) if !value.starts_with('"') && !value.starts_with('\'') => {
            value[..index].trim_end()
        }
        _ => value,
    };
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        value[1..value.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\")
    } else if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
        value[1..value.len() - 1].replace("''", "'")
    } else {
        value.to_string()
    }
}

pub fn is_kebab_slug(value: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= MAX_MEMORY_NAME_CHARS
        && value.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

/// Validate a memory entry (not `MEMORY.md`) written through the API.
pub fn validate_memory_content(content: &str) -> Result<MemoryFrontmatter, MemoryError> {
    if content.len() > MAX_MEMORY_FILE_BYTES {
        return Err(MemoryError::InvalidContent(format!(
            "memory files are limited to {MAX_MEMORY_FILE_BYTES} bytes"
        )));
    }
    let frontmatter = parse_frontmatter(content).ok_or_else(|| {
        MemoryError::InvalidContent(
            "memory content must start with a closed --- YAML frontmatter block".to_string(),
        )
    })?;
    match frontmatter.name.as_deref() {
        Some(name) if is_kebab_slug(name) => {}
        _ => {
            return Err(MemoryError::InvalidContent(format!(
            "frontmatter name must be a kebab-case slug of at most {MAX_MEMORY_NAME_CHARS} chars"
        )))
        }
    }
    match frontmatter.description.as_deref() {
        Some(description)
            if !description.trim().is_empty()
                && description.chars().count() <= MAX_MEMORY_DESCRIPTION_CHARS
                && !description.chars().any(char::is_control) => {}
        _ => {
            return Err(MemoryError::InvalidContent(format!(
                "frontmatter description must be one non-empty line of at most {MAX_MEMORY_DESCRIPTION_CHARS} chars"
            )))
        }
    }
    match frontmatter.memory_type.as_deref() {
        Some(memory_type) if MEMORY_TYPES.contains(&memory_type) => {}
        _ => {
            return Err(MemoryError::InvalidContent(
                "frontmatter metadata.type must be one of user, feedback, project, reference"
                    .to_string(),
            ))
        }
    }
    Ok(frontmatter)
}

/// One `MEMORY.md` index line for an entry.
pub fn index_line(file: &str, frontmatter: &MemoryFrontmatter) -> String {
    let title = frontmatter.name.as_deref().unwrap_or(file);
    match frontmatter.description.as_deref() {
        Some(description) if !description.trim().is_empty() => {
            format!("- [{title}]({file}) — {}", description.trim())
        }
        _ => format!("- [{title}]({file})"),
    }
}

fn index_line_references(line: &str, file: &str) -> bool {
    let line = line.trim_start();
    (line.starts_with("- [") || line.starts_with("* ["))
        && (line.contains(&format!("]({file})")) || line.contains(&format!("](./{file})")))
}

/// Insert or replace the index line for `file`. Returns `None` when unchanged.
pub fn upsert_index_line(index: Option<&str>, file: &str, line: &str) -> Option<String> {
    let index = index.unwrap_or("");
    let mut replaced = false;
    let mut changed = false;
    let mut lines = Vec::new();
    for existing in index.lines() {
        if !replaced && index_line_references(existing, file) {
            replaced = true;
            if existing != line {
                changed = true;
            }
            lines.push(line.to_string());
        } else {
            lines.push(existing.to_string());
        }
    }
    if !replaced {
        while lines.last().is_some_and(|last| last.trim().is_empty()) {
            lines.pop();
        }
        lines.push(line.to_string());
        changed = true;
    }
    changed.then(|| {
        let mut next = lines.join("\n");
        next.push('\n');
        next
    })
}

/// Remove every index line referencing `file`. Returns `None` when unchanged.
pub fn remove_index_lines(index: &str, file: &str) -> Option<String> {
    let kept: Vec<&str> = index
        .lines()
        .filter(|line| !index_line_references(line, file))
        .collect();
    if kept.len() == index.lines().count() {
        return None;
    }
    let mut next = kept.join("\n");
    if !next.is_empty() {
        next.push('\n');
    }
    Some(next)
}

#[derive(Clone, Debug)]
pub struct MemoryStore {
    dir: PathBuf,
}

impl MemoryStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn for_project(project_id: &ProjectId) -> anyhow::Result<Self> {
        Ok(Self::new(project_memory_dir(project_id)?))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Create the directory (agents need it to exist before writing).
    pub fn ensure_dir(&self) -> anyhow::Result<()> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("failed to create memory dir {}", self.dir.display()))
    }

    fn path(&self, file: &MemoryFileName) -> PathBuf {
        self.dir.join(file.as_str())
    }

    /// Serialises cooperating API writers across daemon/CLI processes, including the revision
    /// check and the index RMW. Direct agent file writes do not participate in this advisory lock.
    fn write_lock(&self) -> anyhow::Result<fs::File> {
        self.ensure_dir()?;
        let mut options = fs::OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(self.dir.join(".memory.lock"))?;
        fs2::FileExt::lock_exclusive(&lock).context("lock memory mutations")?;
        Ok(lock)
    }

    fn read_raw(
        &self,
        file: &MemoryFileName,
    ) -> Result<Option<(Vec<u8>, fs::Metadata)>, MemoryError> {
        let path = self.path(file);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(MemoryError::Io(
                    anyhow::Error::new(error).context(format!("failed to stat {}", path.display())),
                ))
            }
        };
        if !metadata.is_file() {
            // Symlinks and directories are never followed or served.
            return Ok(None);
        }
        let bytes =
            fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
        Ok(Some((bytes, metadata)))
    }

    fn document(file: &MemoryFileName, bytes: Vec<u8>, metadata: &fs::Metadata) -> MemoryDocument {
        let revision = content_revision(&bytes);
        let size = bytes.len() as u64;
        let content = String::from_utf8_lossy(&bytes).into_owned();
        let frontmatter = match file {
            MemoryFileName::Index => None,
            MemoryFileName::Entry(_) => parse_frontmatter(&content),
        };
        MemoryDocument {
            file: file.as_str().to_string(),
            content,
            frontmatter,
            revision,
            bytes: size,
            updated_at: modified_rfc3339(metadata),
        }
    }

    pub fn read(&self, file: &MemoryFileName) -> Result<MemoryDocument, MemoryError> {
        let (bytes, metadata) = self.read_raw(file)?.ok_or(MemoryError::NotFound)?;
        Ok(Self::document(file, bytes, &metadata))
    }

    pub fn read_index(&self) -> Result<Option<MemoryDocument>, MemoryError> {
        match self.read(&MemoryFileName::Index) {
            Ok(document) => Ok(Some(document)),
            Err(MemoryError::NotFound) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// All entries (excluding `MEMORY.md`), sorted by file name. Files whose
    /// names do not match the memory file rule are ignored.
    pub fn list(&self) -> Result<Vec<MemoryEntry>, MemoryError> {
        let read_dir = match fs::read_dir(&self.dir) {
            Ok(read_dir) => read_dir,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(MemoryError::Io(anyhow::Error::new(error).context(format!(
                    "failed to list memory dir {}",
                    self.dir.display()
                ))))
            }
        };
        let mut entries = Vec::new();
        for item in read_dir {
            let item = item.with_context(|| format!("failed to list {}", self.dir.display()))?;
            let Some(name) = item.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let file = match MemoryFileName::parse(&name) {
                Ok(MemoryFileName::Entry(name)) => MemoryFileName::Entry(name),
                _ => continue,
            };
            let Some((bytes, metadata)) = self.read_raw(&file)? else {
                continue;
            };
            let document = Self::document(&file, bytes, &metadata);
            let frontmatter = document.frontmatter.unwrap_or_default();
            entries.push(MemoryEntry {
                file: document.file,
                name: frontmatter.name,
                description: frontmatter.description,
                memory_type: frontmatter.memory_type,
                bytes: document.bytes,
                updated_at: document.updated_at,
                revision: document.revision,
            });
        }
        entries.sort_by(|left, right| left.file.cmp(&right.file));
        Ok(entries)
    }

    fn check_expected(
        current: Option<&str>,
        expected: &ExpectedRevision,
    ) -> Result<(), MemoryError> {
        let ok = match expected {
            ExpectedRevision::Any => true,
            ExpectedRevision::Absent => current.is_none(),
            ExpectedRevision::Exact(revision) => current == Some(revision.as_str()),
        };
        if ok {
            Ok(())
        } else {
            Err(MemoryError::RevisionConflict {
                current: current.map(str::to_string),
            })
        }
    }

    /// Write one file. Entries are validated (frontmatter); `MEMORY.md` is
    /// written raw. When `update_index` is set, writing an entry inserts or
    /// replaces its `MEMORY.md` line.
    pub fn write(
        &self,
        file: &MemoryFileName,
        content: &str,
        expected: &ExpectedRevision,
        update_index: bool,
    ) -> Result<MemoryWriteOutcome, MemoryError> {
        let frontmatter = match file {
            MemoryFileName::Index => {
                if content.len() > MAX_MEMORY_INDEX_BYTES {
                    return Err(MemoryError::InvalidContent(format!(
                        "MEMORY.md is limited to {MAX_MEMORY_INDEX_BYTES} bytes"
                    )));
                }
                None
            }
            MemoryFileName::Entry(_) => Some(validate_memory_content(content)?),
        };
        let _guard = self.write_lock()?;
        let current = self
            .read_raw(file)?
            .map(|(bytes, _)| content_revision(&bytes));
        Self::check_expected(current.as_deref(), expected)?;
        self.ensure_dir()?;
        crate::config::atomic::write_private_bytes_atomic(&self.path(file), content.as_bytes())?;
        let mut index_updated = false;
        if let (true, Some(frontmatter)) = (update_index, frontmatter.as_ref()) {
            let index = self
                .read_raw(&MemoryFileName::Index)?
                .map(|(bytes, _)| String::from_utf8_lossy(&bytes).into_owned());
            if let Some(next) = upsert_index_line(
                index.as_deref(),
                file.as_str(),
                &index_line(file.as_str(), frontmatter),
            ) {
                crate::config::atomic::write_private_bytes_atomic(
                    &self.path(&MemoryFileName::Index),
                    next.as_bytes(),
                )?;
                index_updated = true;
            }
        }
        let document = self.read(file)?;
        Ok(MemoryWriteOutcome {
            change: if current.is_some() {
                MemoryChange::Updated
            } else {
                MemoryChange::Created
            },
            document,
            index_updated,
        })
    }

    /// Delete one entry and its index line(s). `MEMORY.md` cannot be deleted.
    pub fn delete(
        &self,
        file: &MemoryFileName,
        expected: &ExpectedRevision,
    ) -> Result<MemoryDeleteOutcome, MemoryError> {
        if *file == MemoryFileName::Index {
            return Err(MemoryError::InvalidContent(
                "MEMORY.md cannot be deleted; write an empty index instead".to_string(),
            ));
        }
        let _guard = self.write_lock()?;
        let current = self
            .read_raw(file)?
            .map(|(bytes, _)| content_revision(&bytes))
            .ok_or(MemoryError::NotFound)?;
        Self::check_expected(Some(&current), expected)?;
        let path = self.path(file);
        fs::remove_file(&path).with_context(|| format!("failed to delete {}", path.display()))?;
        let mut index_updated = false;
        if let Some((bytes, _)) = self.read_raw(&MemoryFileName::Index)? {
            let index = String::from_utf8_lossy(&bytes);
            if let Some(next) = remove_index_lines(&index, file.as_str()) {
                crate::config::atomic::write_private_bytes_atomic(
                    &self.path(&MemoryFileName::Index),
                    next.as_bytes(),
                )?;
                index_updated = true;
            }
        }
        Ok(MemoryDeleteOutcome { index_updated })
    }
}

fn modified_rfc3339(metadata: &fs::Metadata) -> Option<String> {
    metadata
        .modified()
        .ok()
        .map(|time| chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> MemoryStore {
        MemoryStore::new(
            std::env::temp_dir()
                .join(format!("cutex-memory-store-{}", uuid::Uuid::new_v4()))
                .join("memory"),
        )
    }

    const FEEDBACK: &str = "---\nname: test-style\ndescription: Prefer real DB in integration tests\nmetadata:\n  type: feedback\n---\n\nDo not mock the database.\n**Why:** a mock hid a migration bug.\n**How to apply:** integration tests.\n";

    #[test]
    fn memory_store_process_mutation() {
        let Some(dir) = std::env::var_os("CUTEX_MEMORY_LOCK_TEST_DIR") else { return };
        let store = MemoryStore::new(PathBuf::from(dir));
        let mode = std::env::var("CUTEX_MEMORY_LOCK_TEST_MODE").unwrap();
        fs::write(store.dir.join(format!("ready-{mode}")), b"").unwrap();
        if mode == "write" || mode == "delete" {
            let file = MemoryFileName::parse("shared.md").unwrap();
            let expected = ExpectedRevision::Exact(content_revision(FEEDBACK.as_bytes()));
            let conflict = if mode == "write" {
                store.write(&file, &format!("{FEEDBACK}child\n"), &expected, true).map(|_| ()).unwrap_err()
            } else {
                store.delete(&file, &expected).map(|_| ()).unwrap_err()
            };
            assert!(matches!(conflict, MemoryError::RevisionConflict { .. }));
        } else {
            for i in 0..8 {
                let file = MemoryFileName::parse(&format!("{mode}-{i}.md")).unwrap();
                store.write(&file, FEEDBACK, &ExpectedRevision::Absent, true).unwrap();
            }
        }
    }

    fn mutation_child(store: &MemoryStore, mode: &str) -> std::process::Child {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "memory::store::tests::memory_store_process_mutation", "--nocapture"])
            .env("CUTEX_MEMORY_LOCK_TEST_DIR", &store.dir)
            .env("CUTEX_MEMORY_LOCK_TEST_MODE", mode)
            .spawn().unwrap()
    }

    fn wait_ready(store: &MemoryStore, modes: &[&str]) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !modes.iter().all(|mode| store.dir.join(format!("ready-{mode}")).exists()) {
            assert!(std::time::Instant::now() < deadline, "memory children did not reach the mutation lock");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn cross_process_writes_and_deletes_check_revision_under_the_directory_lock() {
        let store = temp_store();
        let file = MemoryFileName::parse("shared.md").unwrap();
        store.write(&file, FEEDBACK, &ExpectedRevision::Absent, true).unwrap();
        let lock = store.write_lock().unwrap();
        let mut children = [mutation_child(&store, "write"), mutation_child(&store, "delete")];
        wait_ready(&store, &["write", "delete"]);
        // Simulate another cooperating writer finishing its critical section while both subprocess
        // mutations wait on the same OS lock. They must compare revisions only after it releases.
        let updated = format!("{FEEDBACK}parent\n");
        crate::config::atomic::write_private_bytes_atomic(&store.path(&file), updated.as_bytes()).unwrap();
        drop(lock);
        for child in &mut children { assert!(child.wait().unwrap().success()); }
        assert_eq!(store.read(&file).unwrap().content, updated);
        fs::remove_dir_all(store.dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn cross_process_entry_writes_preserve_every_index_line() {
        let store = temp_store();
        let lock = store.write_lock().unwrap();
        let mut children = [mutation_child(&store, "a"), mutation_child(&store, "b")];
        wait_ready(&store, &["a", "b"]);
        drop(lock);
        for child in &mut children { assert!(child.wait().unwrap().success()); }
        assert_eq!(store.list().unwrap().len(), 16);
        let index = store.read_index().unwrap().unwrap().content;
        for mode in ["a", "b"] { for i in 0..8 {
            assert!(index.contains(&format!("]({mode}-{i}.md)")), "{index}");
        } }
        fs::remove_dir_all(store.dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn project_key_is_safe_deterministic_and_case_distinct() {
        let key = project_memory_key("Team:Alpha");
        assert!(key.starts_with("team-alpha-"), "{key}");
        assert_eq!(key, project_memory_key("Team:Alpha"));
        assert_ne!(key, project_memory_key("team:alpha"));
        assert!(key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'));
        assert!(project_memory_key("::").starts_with("project-"));
    }

    #[test]
    fn file_names_reject_traversal_and_odd_names() {
        assert_eq!(
            MemoryFileName::parse("MEMORY.md").unwrap(),
            MemoryFileName::Index
        );
        assert!(MemoryFileName::parse("feedback_testing.md").is_ok());
        assert!(MemoryFileName::parse("a.b-c.md").is_ok());
        for bad in [
            "../x.md",
            "..\\x.md",
            "x/y.md",
            ".hidden.md",
            "Upper.md",
            "x.txt",
            ".md",
            "memory.MD",
            "x.md.",
            "",
            "a b.md",
            "C:x.md",
            "x%2F.md",
        ] {
            assert!(MemoryFileName::parse(bad).is_err(), "{bad}");
        }
        assert!(MemoryFileName::parse(&format!("{}.md", "a".repeat(126))).is_err());
    }

    #[test]
    fn frontmatter_parses_metadata_type_quotes_and_fallback() {
        let parsed = parse_frontmatter(FEEDBACK).unwrap();
        assert_eq!(parsed.name.as_deref(), Some("test-style"));
        assert_eq!(parsed.memory_type.as_deref(), Some("feedback"));
        let quoted = "---\nname: \"x-y\"\ndescription: 'it''s fine' \ntype: user\n---\nbody";
        let parsed = parse_frontmatter(quoted).unwrap();
        assert_eq!(parsed.name.as_deref(), Some("x-y"));
        assert_eq!(parsed.description.as_deref(), Some("it's fine"));
        assert_eq!(parsed.memory_type.as_deref(), Some("user"));
        assert!(parse_frontmatter("no frontmatter").is_none());
        assert!(parse_frontmatter("---\nname: open\n").is_none());
    }

    #[test]
    fn validation_requires_slug_description_and_type() {
        assert!(validate_memory_content(FEEDBACK).is_ok());
        for bad in [
            "body only",
            "---\nname: Bad Name\ndescription: d\nmetadata:\n  type: user\n---\n",
            "---\nname: ok\ndescription: \nmetadata:\n  type: user\n---\n",
            "---\nname: ok\ndescription: d\nmetadata:\n  type: secret\n---\n",
            "---\nname: ok\ndescription: d\n---\n",
        ] {
            assert!(validate_memory_content(bad).is_err(), "{bad}");
        }
        let huge = format!("{FEEDBACK}{}", "x".repeat(MAX_MEMORY_FILE_BYTES));
        assert!(validate_memory_content(&huge).is_err());
    }

    #[test]
    fn index_lines_upsert_and_remove() {
        let fm = parse_frontmatter(FEEDBACK).unwrap();
        let line = index_line("test-style.md", &fm);
        assert_eq!(
            line,
            "- [test-style](test-style.md) — Prefer real DB in integration tests"
        );
        let created = upsert_index_line(None, "test-style.md", &line).unwrap();
        assert_eq!(created, format!("{line}\n"));
        assert!(upsert_index_line(Some(&created), "test-style.md", &line).is_none());
        let other = "- [other](other.md) — o\n";
        let appended = upsert_index_line(Some(other), "test-style.md", &line).unwrap();
        assert_eq!(appended, format!("- [other](other.md) — o\n{line}\n"));
        let replaced = upsert_index_line(
            Some(&appended),
            "test-style.md",
            "- [t](test-style.md) — new",
        )
        .unwrap();
        assert!(replaced.contains("- [t](test-style.md) — new"));
        assert!(!replaced.contains("Prefer real DB"));
        let removed = remove_index_lines(&replaced, "test-style.md").unwrap();
        assert_eq!(removed, other);
        assert!(remove_index_lines(other, "test-style.md").is_none());
    }

    #[test]
    fn store_round_trip_with_revisions_and_index_maintenance() {
        let store = temp_store();
        assert!(store.list().unwrap().is_empty());
        assert!(store.read_index().unwrap().is_none());
        let file = MemoryFileName::parse("test-style.md").unwrap();

        let created = store
            .write(&file, FEEDBACK, &ExpectedRevision::Absent, true)
            .unwrap();
        assert_eq!(created.change, MemoryChange::Created);
        assert!(created.index_updated);
        assert_eq!(
            created.document.revision,
            content_revision(FEEDBACK.as_bytes())
        );
        let index = store.read_index().unwrap().unwrap();
        assert!(index.content.contains("](test-style.md)"));
        assert!(index.frontmatter.is_none());

        let conflict = store
            .write(&file, FEEDBACK, &ExpectedRevision::Absent, true)
            .unwrap_err();
        assert!(matches!(
            conflict,
            MemoryError::RevisionConflict { current: Some(_) }
        ));
        let stale = store
            .write(
                &file,
                FEEDBACK,
                &ExpectedRevision::Exact("sha256:00".into()),
                true,
            )
            .unwrap_err();
        assert!(matches!(stale, MemoryError::RevisionConflict { .. }));

        let updated_content = FEEDBACK.replace("Prefer real DB", "Use the real DB");
        let updated = store
            .write(
                &file,
                &updated_content,
                &ExpectedRevision::Exact(created.document.revision.clone()),
                true,
            )
            .unwrap();
        assert_eq!(updated.change, MemoryChange::Updated);
        assert!(updated.index_updated);
        let index = store.read_index().unwrap().unwrap().content;
        assert!(index.contains("Use the real DB") && !index.contains("Prefer real DB"));
        assert_eq!(index.lines().count(), 1);

        let entries = store.list().unwrap();
        assert_eq!(entries.len(), 1, "MEMORY.md is not an entry");
        assert_eq!(entries[0].file, "test-style.md");
        assert_eq!(entries[0].name.as_deref(), Some("test-style"));
        assert_eq!(entries[0].memory_type.as_deref(), Some("feedback"));
        assert_eq!(entries[0].bytes, updated_content.len() as u64);
        assert!(entries[0].updated_at.is_some());

        // Unknown or unsafe files in the directory are ignored by list.
        fs::write(store.dir().join("Notes.txt"), "x").unwrap();
        assert_eq!(store.list().unwrap().len(), 1);

        assert!(matches!(
            store.write(&file, "no frontmatter", &ExpectedRevision::Any, true),
            Err(MemoryError::InvalidContent(_))
        ));
        assert!(matches!(
            store.delete(&MemoryFileName::Index, &ExpectedRevision::Any),
            Err(MemoryError::InvalidContent(_))
        ));
        assert!(matches!(
            store.delete(&file, &ExpectedRevision::Exact("sha256:00".into())),
            Err(MemoryError::RevisionConflict { .. })
        ));
        let deleted = store
            .delete(&file, &ExpectedRevision::Exact(updated.document.revision))
            .unwrap();
        assert!(deleted.index_updated);
        assert!(matches!(store.read(&file), Err(MemoryError::NotFound)));
        assert_eq!(store.read_index().unwrap().unwrap().content, "");
        assert!(matches!(
            store.delete(&file, &ExpectedRevision::Any),
            Err(MemoryError::NotFound)
        ));

        let raw = store
            .write(
                &MemoryFileName::Index,
                "# anything\n",
                &ExpectedRevision::Any,
                true,
            )
            .unwrap();
        assert_eq!(raw.change, MemoryChange::Updated);
        assert!(!raw.index_updated);
        fs::remove_dir_all(store.dir().parent().unwrap()).unwrap();
    }
}
