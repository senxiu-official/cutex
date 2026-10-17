//! Publishing a file as a version (MCP `artifact_publish`, owner `POST
//! /v2/artifact-store`, auto capture): path rules, reading / hashing, git
//! provenance, the store write, the quota notice and the events.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use super::host::{ArtifactHost, QUOTA_NOTIFIED_KEY};
use super::paths::{self, PathError, Resolved};
use super::store::{ArtifactStore, Content, NewVersion, Outcome, StoreError};
use super::{GitInfo, Producer, Scope};
use crate::config::artifacts::ArtifactSettings;

#[derive(Clone, Debug)]
pub struct PublishRequest {
    pub scope: Scope,
    pub path: String,
    /// Relative paths are joined to this.
    pub base: Option<PathBuf>,
    /// The file must be inside one of these.
    pub roots: Vec<PathBuf>,
    /// Default names are relative to the first of these containing the file.
    pub name_bases: Vec<PathBuf>,
    pub name: Option<String>,
    pub kind: Option<String>,
    pub note: Option<String>,
    pub producer: Producer,
}

#[derive(Debug)]
pub enum PublishError {
    Invalid(String),
    Forbidden(String),
    QuotaFull { used: u64, quota: u64 },
    Store(anyhow::Error),
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) | Self::Forbidden(message) => f.write_str(message),
            Self::QuotaFull { used, quota } => write!(f, "the artifact store is full ({used} of {quota} bytes used)"),
            Self::Store(error) => write!(f, "the artifact store is unavailable: {error:#}"),
        }
    }
}

impl From<PathError> for PublishError {
    fn from(error: PathError) -> Self {
        match error {
            PathError::Invalid(message) => Self::Invalid(message),
            PathError::Forbidden(message) => Self::Forbidden(message),
        }
    }
}

impl From<StoreError> for PublishError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::QuotaFull { used, quota } => Self::QuotaFull { used, quota },
            StoreError::Invalid(message) => Self::Invalid(message),
            other => Self::Store(anyhow::anyhow!("{other}")),
        }
    }
}

/// Stream a file's sha256.
pub fn hash_file(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Read a resolved file: copied when it fits `maxBlobBytes`, else
/// registered (hashed up to `hashExternalMaxBytes` when `hash_external`).
pub fn read_content(resolved: &Resolved, settings: &ArtifactSettings, hash_external: bool) -> Result<(Content, Vec<u8>), PublishError> {
    let cannot = |error: std::io::Error| PublishError::Invalid(format!("cannot read {}: {error}", resolved.display()));
    if resolved.bytes <= settings.max_blob_bytes {
        let mut bytes = Vec::with_capacity(resolved.bytes as usize);
        std::fs::File::open(&resolved.path)
            .map_err(cannot)?
            .take(settings.max_blob_bytes + 1)
            .read_to_end(&mut bytes)
            .map_err(cannot)?;
        if bytes.len() as u64 <= settings.max_blob_bytes {
            let sha256 = crate::asks::sha256_hex(&bytes);
            let sample = bytes[..bytes.len().min(8192)].to_vec();
            return Ok((Content::Blob { bytes, sha256 }, sample));
        }
    }
    let sha256 = if hash_external && resolved.bytes <= settings.hash_external_max_bytes {
        Some(hash_file(&resolved.path).map_err(cannot)?)
    } else {
        None
    };
    let mut sample = Vec::new();
    if let Ok(file) = std::fs::File::open(&resolved.path) {
        let _ = file.take(8192).read_to_end(&mut sample);
    }
    Ok((Content::External { sha256, bytes: resolved.bytes }, sample))
}

/// Git provenance of files, cached per directory and work tree.
///
/// The work tree is agent-controlled (its `.git/config`, `info/attributes`
/// and any nested `.git` an agent plants), while this runs unsandboxed in
/// the management / bus process. So only object reads run: no `git status`
/// (it would run `core.fsmonitor`, clean / process filters and promisor
/// fetches the repository configures), transports are refused and lazy
/// fetches are off. `dirty` = the file differs from its `HEAD` version (or
/// is not in `HEAD`); compared without filters, so a CRLF checkout of an LF
/// blob still counts as clean.
#[derive(Default)]
pub struct GitCache {
    /// Directory → work tree root (None: not in a work tree).
    roots: HashMap<PathBuf, Option<PathBuf>>,
    commits: HashMap<PathBuf, Option<String>>,
}

/// Files over this size are reported dirty without hashing.
const GIT_COMPARE_MAX_BYTES: u64 = 64 * 1024 * 1024;

impl GitCache {
    /// `{commit, dirty}` of the work tree holding `file`, `None` outside a
    /// work tree or without a commit or git.
    pub fn info(&mut self, file: &Path) -> Option<GitInfo> {
        let dir = file.parent()?.to_path_buf();
        let root = match self.roots.get(&dir) {
            Some(root) => root.clone(),
            None => {
                let root = dir.ancestors().find(|candidate| candidate.join(".git").exists()).map(Path::to_path_buf);
                self.roots.insert(dir, root.clone());
                root
            }
        }?;
        let commit = match self.commits.get(&root) {
            Some(commit) => commit.clone(),
            None => {
                let commit = head_commit(&root);
                self.commits.insert(root.clone(), commit.clone());
                commit
            }
        }?;
        let dirty = !matches_head(&root, file);
        Some(GitInfo { commit, dirty })
    }
}

fn git_command(dir: &Path, args: &[&str]) -> std::process::Command {
    let mut command = std::process::Command::new("git");
    command
        .current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("LC_ALL", "C")
        .args(["-c", "core.quotepath=false", "-c", "color.ui=false", "-c", "core.fsmonitor=false"])
        .args(["-c", "protocol.allow=never"]);
    for protocol in ["file", "ext", "ssh", "git", "http", "https"] {
        command.args(["-c", &format!("protocol.{protocol}.allow=never")]);
    }
    command.args(args).stderr(std::process::Stdio::null()).stdout(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    command
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = git_command(dir, args).stdin(std::process::Stdio::null()).output().ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// `git hash-object --no-filters --stdin` of `bytes` (the repository's
/// object format).
fn hash_object(dir: &Path, bytes: &[u8]) -> Option<String> {
    use std::io::Write as _;
    let mut child = git_command(dir, &["hash-object", "--no-filters", "--stdin"]).stdin(std::process::Stdio::piped()).spawn().ok()?;
    let mut stdin = child.stdin.take()?;
    let written = stdin.write_all(bytes);
    drop(stdin);
    let output = child.wait_with_output().ok()?;
    (written.is_ok() && output.status.success()).then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn head_commit(root: &Path) -> Option<String> {
    let commit = git(root, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])?;
    (commit.len() >= 7 && commit.bytes().all(|b| b.is_ascii_hexdigit())).then_some(commit)
}

/// Whether `file` has the content of its `HEAD` blob (raw, or with CRLF
/// line ends of an LF blob).
fn matches_head(root: &Path, file: &Path) -> bool {
    let Some(rel) = paths::relative(file, root).filter(|rel| !rel.is_empty()) else {
        return false;
    };
    let Some(blob) = git(root, &["rev-parse", "--verify", "--quiet", &format!("HEAD:{rel}")]) else {
        return false;
    };
    let Ok(metadata) = std::fs::metadata(file) else {
        return false;
    };
    if !metadata.is_file() || metadata.len() > GIT_COMPARE_MAX_BYTES {
        return false;
    }
    let Ok(bytes) = std::fs::read(file) else {
        return false;
    };
    if hash_object(root, &bytes).as_deref() == Some(blob.as_str()) {
        return true;
    }
    if bytes.windows(2).any(|pair| pair == b"\r\n") {
        let mut lf = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
                index += 1;
                continue;
            }
            lf.push(bytes[index]);
            index += 1;
        }
        return hash_object(root, &lf).as_deref() == Some(blob.as_str());
    }
    false
}

/// Validate and read a publish request into a [`NewVersion`].
pub fn prepare(request: &PublishRequest, settings: &ArtifactSettings, git: &mut GitCache) -> Result<NewVersion, PublishError> {
    if let Some(kind) = &request.kind {
        if !super::is_kind(kind) {
            return Err(PublishError::Invalid(format!("kind: one of {}", super::KINDS.join(", "))));
        }
    }
    let name = match &request.name {
        Some(name) => Some(
            super::clean_name(name).ok_or_else(|| PublishError::Invalid(format!("name: 1-{} characters", super::MAX_NAME_CHARS)))?,
        ),
        None => None,
    };
    let resolved = paths::resolve(&request.path, request.base.as_deref(), &request.roots)?;
    let name = name.unwrap_or_else(|| paths::default_name(&resolved.path, &request.name_bases));
    let (content, sample) = read_content(&resolved, settings, true)?;
    let source_path = resolved.display();
    Ok(NewVersion {
        scope: request.scope.clone(),
        default_kind: paths::default_kind_of(&name, &source_path).to_string(),
        media_type: paths::media_type(&source_path, &name, &sample),
        name,
        kind: request.kind.clone(),
        content,
        source_path,
        source_mtime_ms: resolved.mtime_ms,
        note: super::clean_note(request.note.as_deref()),
        git: git.info(&resolved.path),
        producer: request.producer.clone(),
    })
}

/// Write a prepared version: quota notice once per full episode, events.
pub fn commit(host: &dyn ArtifactHost, store: &ArtifactStore, new: NewVersion, now: DateTime<Utc>) -> Result<Outcome, PublishError> {
    let settings = host.settings();
    let name = new.name.clone();
    let session = new.producer.session_id.clone();
    match store.publish(new, settings.quota_bytes, now) {
        Ok(outcome) => {
            if store.meta(QUOTA_NOTIFIED_KEY).ok().flatten().is_some() {
                let _ = store.set_meta(QUOTA_NOTIFIED_KEY, None);
            }
            emit_outcome(host, &outcome);
            Ok(outcome)
        }
        Err(StoreError::QuotaFull { used, quota }) => {
            if store.meta(QUOTA_NOTIFIED_KEY).ok().flatten().is_none() {
                let _ = store.set_meta(QUOTA_NOTIFIED_KEY, Some(&crate::asks::now_rfc3339(now)));
                host.quota_full(session.as_deref(), &name, used, quota);
            }
            Err(PublishError::QuotaFull { used, quota })
        }
        Err(error) => Err(error.into()),
    }
}

/// [`prepare`] + [`commit`].
pub fn publish(host: &dyn ArtifactHost, request: &PublishRequest, now: DateTime<Utc>) -> Result<Outcome, PublishError> {
    let settings = host.settings();
    let store = host.store().map_err(PublishError::Store)?;
    let new = prepare(request, &settings, &mut GitCache::default())?;
    commit(host, &store, new, now)
}

pub fn published_event(outcome: &Outcome) -> Value {
    json!({
        "artifactId": outcome.artifact.artifact_id,
        "version": outcome.version.version,
        "scope": outcome.artifact.scope,
        "name": outcome.artifact.name,
        "kind": outcome.artifact.kind,
        "producer": outcome.version.producer,
        "newVersion": outcome.new_version,
    })
}

pub fn updated_event(artifact: &super::ArtifactSummary, change: &str) -> Value {
    json!({
        "artifactId": artifact.artifact_id,
        "scope": artifact.scope,
        "change": change,
        "revision": artifact.revision,
    })
}

/// `published` (also for a no-op publish) plus `updated` for the metadata
/// changes the publish made.
pub fn emit_outcome(host: &dyn ArtifactHost, outcome: &Outcome) {
    host.emit(super::EVENT_PUBLISHED, published_event(outcome));
    for change in &outcome.changes {
        host.emit(super::EVENT_UPDATED, updated_event(&outcome.artifact, change));
    }
}

#[cfg(test)]
mod tests {
    use super::super::host::tests::TestHost;
    use super::*;

    fn request(_host: &TestHost, workspace: &Path, path: &str) -> PublishRequest {
        PublishRequest {
            scope: Scope::project("proj-a"),
            path: path.into(),
            base: Some(workspace.to_path_buf()),
            roots: vec![workspace.to_path_buf()],
            name_bases: vec![workspace.to_path_buf()],
            name: None,
            kind: None,
            note: Some(" first ".into()),
            producer: super::super::store::tests::agent("cutex.a"),
        }
    }

    /// The desktop's 「从工作区发布…」 sends an owner label as the name; the
    /// media type and default kind still follow the published file.
    #[test]
    fn a_label_name_keeps_the_files_media_type_and_kind() {
        let host = TestHost::new("publish-label");
        let workspace = host.workspace("ws");
        std::fs::create_dir_all(workspace.join("out")).unwrap();
        std::fs::write(workspace.join("out/page.html"), "<!DOCTYPE html>\n<html><body><h1>Hi</h1></body></html>\n").unwrap();
        std::fs::write(workspace.join("out/paper.pdf"), b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n1 0 obj\n").unwrap();
        std::fs::write(workspace.join("out/photo.JPG"), [0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00]).unwrap();
        let now = Utc::now();
        for (path, label, media, kind) in [
            ("out/page.html", "round2-html-artifact", "text/html", "report"),
            ("out/paper.pdf", "round2-pdf-artifact", "application/pdf", "report"),
            ("out/photo.JPG", "Holiday photo", "image/jpeg", "figure"),
        ] {
            let mut named = request(&host, &workspace, path);
            named.name = Some(label.into());
            let published = publish(&host, &named, now).unwrap();
            assert_eq!((published.version.media_type.as_str(), published.artifact.kind.as_str()), (media, kind), "{path}");
        }
    }

    #[test]
    fn publishes_blobs_externals_and_emits_events() {
        let host = TestHost::new("publish");
        let workspace = host.workspace("ws");
        std::fs::create_dir_all(workspace.join("reports")).unwrap();
        std::fs::write(workspace.join("reports/r.md"), "# R\n").unwrap();
        let now = Utc::now();
        let first = publish(&host, &request(&host, &workspace, "reports/r.md"), now).unwrap();
        assert_eq!(first.artifact.name, "reports/r.md");
        assert_eq!(first.artifact.kind, "report");
        assert_eq!(first.version.media_type, "text/markdown");
        assert_eq!(first.version.note.as_deref(), Some("first"));
        assert_eq!(first.version.storage, "blob");
        assert!(first.version.source_path.ends_with("r.md"));
        assert!(!first.version.source_path.starts_with(r"\\?\"));
        assert_eq!(host.methods(), vec![super::super::EVENT_PUBLISHED]);
        let again = publish(&host, &request(&host, &workspace, "reports/r.md"), now).unwrap();
        assert!(!again.new_version);
        assert_eq!(host.events.borrow()[1].1["newVersion"], false);
        // Over maxBlobBytes: registered, hashed (under hashExternalMaxBytes).
        host.settings.borrow_mut().max_blob_bytes = 4;
        std::fs::write(workspace.join("reports/big.csv"), "a,b\n1,2\n").unwrap();
        let big = publish(&host, &request(&host, &workspace, "reports/big.csv"), now).unwrap();
        assert_eq!((big.version.storage.as_str(), big.version.bytes), ("external", 8));
        assert!(big.version.sha256.is_some());
        assert_eq!(big.artifact.kind, "table");
        host.settings.borrow_mut().hash_external_max_bytes = 0;
        std::fs::write(workspace.join("reports/big.csv"), "a,b\n3,4\n").unwrap();
        let unhashed = publish(&host, &request(&host, &workspace, "reports/big.csv"), now).unwrap();
        assert!(unhashed.version.sha256.is_none());
        assert!(unhashed.new_version);
        // Explicit name / kind; bad kind; forbidden path.
        let mut named = request(&host, &workspace, "reports/r.md");
        named.name = Some("Regression".into());
        named.kind = Some("plan".into());
        let named = publish(&host, &named, now).unwrap();
        assert_eq!((named.artifact.name.as_str(), named.artifact.kind.as_str()), ("Regression", "plan"));
        assert_eq!(named.version.media_type, "text/markdown");
        let mut bad = request(&host, &workspace, "reports/r.md");
        bad.kind = Some("poem".into());
        assert!(matches!(publish(&host, &bad, now), Err(PublishError::Invalid(_))));
        let outside = host.workspace("outside");
        std::fs::write(outside.join("x.md"), "x").unwrap();
        let escape = request(&host, &workspace, &outside.join("x.md").display().to_string());
        assert!(matches!(publish(&host, &escape, now), Err(PublishError::Forbidden(_))));
    }

    #[test]
    fn quota_full_notifies_once_per_episode() {
        let host = TestHost::new("quota");
        let workspace = host.workspace("ws");
        host.settings.borrow_mut().quota_bytes = 10;
        std::fs::write(workspace.join("a.md"), "12345678").unwrap();
        std::fs::write(workspace.join("b.md"), "abcdefgh").unwrap();
        std::fs::write(workspace.join("c.md"), "ABCDEFGH").unwrap();
        let now = Utc::now();
        publish(&host, &request(&host, &workspace, "a.md"), now).unwrap();
        assert!(matches!(publish(&host, &request(&host, &workspace, "b.md"), now), Err(PublishError::QuotaFull { .. })));
        assert!(matches!(publish(&host, &request(&host, &workspace, "c.md"), now), Err(PublishError::QuotaFull { .. })));
        assert_eq!(host.quota_notices.borrow().len(), 1, "once per full episode");
        assert_eq!(host.quota_notices.borrow()[0], (Some("cutex.a".to_string()), "b.md".to_string()));
        host.settings.borrow_mut().quota_bytes = 100;
        publish(&host, &request(&host, &workspace, "b.md"), now).unwrap();
        host.settings.borrow_mut().quota_bytes = 16;
        assert!(publish(&host, &request(&host, &workspace, "c.md"), now).is_err());
        assert_eq!(host.quota_notices.borrow().len(), 2, "a new episode notifies again");
    }

    #[test]
    fn git_provenance_of_the_work_tree() {
        let host = TestHost::new("git");
        let workspace = host.workspace("repo");
        let ok = |args: &[&str]| {
            std::process::Command::new("git")
                .current_dir(&workspace)
                .args(args)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        };
        if !ok(&["init", "-q"]) {
            eprintln!("git unavailable; skipped");
            return;
        }
        std::fs::write(workspace.join("a.md"), "a").unwrap();
        let mut cache = GitCache::default();
        assert_eq!(cache.info(&workspace.join("a.md")), None, "no commit yet");
        assert!(ok(&["add", "a.md"]));
        assert!(ok(&["-c", "user.name=t", "-c", "user.email=t@example.invalid", "-c", "commit.gpgsign=false", "commit", "-q", "-m", "init"]));
        let info = GitCache::default().info(&workspace.join("a.md")).unwrap();
        assert!(!info.dirty);
        // A CRLF checkout of the LF blob is clean; an untracked file is not.
        std::fs::write(workspace.join("a.md"), "a").unwrap();
        std::fs::write(workspace.join("b.md"), "x\ny\n").unwrap();
        assert!(ok(&["add", "b.md"]));
        assert!(ok(&["-c", "user.name=t", "-c", "user.email=t@example.invalid", "-c", "commit.gpgsign=false", "commit", "-q", "-m", "b"]));
        std::fs::write(workspace.join("b.md"), "x\r\ny\r\n").unwrap();
        assert!(!GitCache::default().info(&workspace.join("b.md")).unwrap().dirty);
        std::fs::write(workspace.join("new.md"), "n").unwrap();
        assert!(GitCache::default().info(&workspace.join("new.md")).unwrap().dirty);
        std::fs::write(workspace.join("a.md"), "changed").unwrap();
        // Programs the (agent-controlled) repository configures never run.
        let marker = workspace.join("pwned");
        let payload = "echo x > pwned; cat";
        assert!(ok(&["config", "filter.evil.clean", payload]));
        assert!(ok(&["config", "core.fsmonitor", "echo x > pwned"]));
        std::fs::write(workspace.join(".gitattributes"), "*.md filter=evil\n").unwrap();
        assert!(GitCache::default().info(&workspace.join("a.md")).unwrap().dirty);
        assert!(!marker.exists(), "provenance ran a repository-configured program");
        let outside = host.workspace("plain");
        std::fs::write(outside.join("b.md"), "b").unwrap();
        let in_plain = GitCache::default().info(&outside.join("b.md"));
        // The temp dir may itself sit in a work tree on some hosts.
        if let Some(in_plain) = in_plain {
            assert_ne!(in_plain.commit, info.commit);
        }
    }
}
