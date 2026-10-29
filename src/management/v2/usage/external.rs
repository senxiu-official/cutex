//! Extra usage sources (capability `usage-external-sources`): token usage of
//! the owner's own Claude Code (`~/.claude`), plain Codex CLI (`~/.codex`)
//! and Zhipu ZCode (`~/.zcode`, capability `usage-source-zcode`) runs that
//! never went through cutex, imported into a separate ledger.
//!
//! - Config: `config.json` `usage.extraSources` (`/v2/usage/sources`).
//! - Rows: `runtime/management-v2/usage-ledger-external/<id>/YYYY-MM.jsonl`,
//!   merged into `GET /v2/usage` by the reader (cutex's own rows win on an
//!   entry id clash). Deleting a source with `purge` removes that directory.
//! - Scan state: `<same dir>/scan-state.json` (byte offset of the last
//!   complete line per file, keyed by relative path + length / mtime / inode).
//! - Only `projects/**/*.jsonl` (Claude Code) and `sessions/**/rollout-*.jsonl`
//!   + `archived_sessions/rollout-*.jsonl` (Codex) are read, and for ZCode
//!   only the token / id columns of `cli/db/db.sqlite` `model_usage`
//!   (read-only, creates no files, see `external_zcode.rs`); credentials, auth and
//!   settings files never are. Line contents are never logged.
//!
//! Claude Code: one row per `message.id` (streamed content blocks repeat the
//! id; the last line wins), settled once another message / user line follows
//! or the line is at least [`SETTLE_SECS`] old. Token mapping as the Claude
//! adapter: input = plain + cache creation + cache read.
//!
//! Codex: `token_usage_record` rows (entry id per `response_id`); rollouts
//! without them fall back to `event_msg` `token_count` (`last_token_usage`,
//! counted only when the cumulative total moves). Sessions whose thread id is
//! already in cutex's ledger, or whose originator is cutex, are skipped.

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::fs;
use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::Context;
use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use serde_json::Value;

use crate::config::usage_pricing::ExtraUsageSource;

use super::model::parse_timestamp;
use super::model::TokenUsageBreakdown;
use super::model::UsageAttribution;
use super::model::UsageLedgerEntry;
use super::model::UsageSample;
use super::model::UsageSampleSource;
use super::model::UsageTokenSource;
use super::store;

pub const CAPABILITY: &str = "usage-external-sources";
/// The `zcode` source kind (Zhipu ZCode's local usage database).
pub const ZCODE_CAPABILITY: &str = "usage-source-zcode";
/// A row younger than this (and not followed by another message) waits for
/// the next scan: streamed Claude lines of one message may still change.
pub const SETTLE_SECS: i64 = 60;
pub const SCAN_INTERVAL: Duration = Duration::from_secs(5 * 60);
const FIRST_SCAN_DELAY: Duration = Duration::from_secs(30);
const STATE_FILE: &str = "scan-state.json";
const STATE_VERSION: u8 = 1;
const MAX_SOURCES: usize = 16;
const MAX_LABEL_CHARS: usize = 64;
const MAX_PATH_BYTES: usize = 1024;
const MAX_ERRORS_KEPT: usize = 5;
/// Directory depth walked below `projects/` / `sessions/`.
const MAX_WALK_DEPTH: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    ClaudeCode,
    Codex,
    /// Zhipu ZCode: `cli/db/db.sqlite` `model_usage` (`external_zcode.rs`).
    Zcode,
}

impl SourceKind {
    pub const ALL: [Self; 3] = [Self::ClaudeCode, Self::Codex, Self::Zcode];

    pub fn key(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Zcode => "zcode",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.key() == value)
    }

    fn display(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
            Self::Zcode => "ZCode",
        }
    }

    /// Default directory under the user's home (`.claude` / `.codex` / `.zcode`).
    fn home_dir_name(self) -> &'static str {
        match self {
            Self::ClaudeCode => ".claude",
            Self::Codex => ".codex",
            Self::Zcode => ".zcode",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Billing {
    Subscription,
    Api,
}

impl Billing {
    pub const ALL: [Self; 2] = [Self::Subscription, Self::Api];

    pub fn key(self) -> &'static str {
        match self {
            Self::Subscription => "subscription",
            Self::Api => "api",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|billing| billing.key() == value)
    }
}

/// A configured source with every field valid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceSpec {
    pub id: String,
    pub kind: SourceKind,
    pub path: PathBuf,
    pub label: String,
    pub billing: Billing,
    pub enabled: bool,
}

impl SourceSpec {
    /// `None` for an entry this build cannot use (unknown kind, bad id ...).
    pub fn from_config(entry: &ExtraUsageSource) -> Option<Self> {
        validate_id(&entry.id).ok()?;
        let path = PathBuf::from(&entry.path);
        path.is_absolute().then_some(())?;
        Some(Self {
            id: entry.id.clone(),
            kind: SourceKind::parse(&entry.kind)?,
            path,
            label: if entry.label.trim().is_empty() {
                SourceKind::parse(&entry.kind)?.display().to_string()
            } else {
                entry.label.clone()
            },
            billing: Billing::parse(&entry.billing).unwrap_or(Billing::Subscription),
            enabled: entry.enabled,
        })
    }

    fn to_config(&self) -> ExtraUsageSource {
        ExtraUsageSource {
            id: self.id.clone(),
            kind: self.kind.key().to_string(),
            path: self.path.to_string_lossy().into_owned(),
            label: self.label.clone(),
            billing: self.billing.key().to_string(),
            enabled: self.enabled,
        }
    }
}

pub fn validate_id(id: &str) -> Result<(), String> {
    let mut chars = id.chars();
    let valid = id.len() <= 40
        && chars.next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if valid {
        Ok(())
    } else {
        Err("id must be 1-40 characters of a-z, 0-9 and -, starting with a letter or digit".into())
    }
}

pub fn validate_label(label: &str) -> Result<String, String> {
    let label = label.trim();
    if label.is_empty() || label.chars().count() > MAX_LABEL_CHARS || label.chars().any(char::is_control) {
        return Err(format!("label must be 1-{MAX_LABEL_CHARS} characters without control characters"));
    }
    Ok(label.to_string())
}

/// Absolute, existing directory, symlinks resolved. `~` / `~/…` expand to
/// the user's home.
pub fn normalize_source_path(raw: &str) -> Result<PathBuf, String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > MAX_PATH_BYTES || raw.chars().any(char::is_control) {
        return Err(format!("path must be 1-{MAX_PATH_BYTES} bytes without control characters"));
    }
    let expanded = if raw == "~" {
        crate::config::paths::home_dir().ok_or("the home directory is unknown")?
    } else if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        crate::config::paths::home_dir().ok_or("the home directory is unknown")?.join(rest)
    } else {
        PathBuf::from(raw)
    };
    if !expanded.is_absolute() {
        return Err("path must be absolute".into());
    }
    let canonical = expanded
        .canonicalize()
        .map_err(|_| "path does not exist or is not readable".to_string())?;
    if !canonical.is_dir() {
        return Err("path must be a directory".into());
    }
    Ok(strip_verbatim(canonical))
}

/// Windows `canonicalize` yields `\\?\C:\…`; store the plain form.
fn strip_verbatim(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            if !rest.starts_with("UNC\\") {
                return PathBuf::from(rest.to_string());
            }
        }
    }
    path
}

fn comparable(path: &Path) -> String {
    let resolved = path.canonicalize().map(strip_verbatim).unwrap_or_else(|_| path.to_path_buf());
    let mut text = resolved.to_string_lossy().replace('\\', "/");
    while text.len() > 1 && text.ends_with('/') {
        text.pop();
    }
    if cfg!(windows) {
        text = text.to_lowercase();
    }
    text
}

/// `a` equals, contains or is inside `b`.
pub fn paths_overlap(a: &Path, b: &Path) -> bool {
    let (a, b) = (comparable(a), comparable(b));
    let inside = |inner: &str, outer: &str| {
        inner == outer || outer == "/" || inner.starts_with(&format!("{outer}/"))
    };
    inside(&a, &b) || inside(&b, &a)
}

/// Directories whose runs cutex already records itself: its home (profiles,
/// `codex-home`, Claude account homes) and the Claude deployment's login
/// home (it may live outside the cutex home).
pub fn cutex_owned_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(dir) = crate::config::paths::config_dir() {
        roots.push(dir);
    }
    if let Ok(dir) = crate::config::paths::host_codex_home_dir() {
        roots.push(dir);
    }
    if let Ok(dir) = crate::runtime::claude_accounts::homes_root() {
        roots.push(dir);
    }
    if let Ok(deployment) = crate::runtime::claude::ClaudeDeployment::selected() {
        roots.push(deployment.claude_home);
    }
    roots
}

#[derive(Debug, PartialEq, Eq)]
pub enum OverlapError {
    /// Equal to / inside / containing a directory cutex records itself.
    Cutex(PathBuf),
    /// Overlaps another configured source.
    Source(String),
}

pub fn check_overlap(
    candidate: &Path,
    owned: &[PathBuf],
    others: &[(String, PathBuf)],
) -> Result<(), OverlapError> {
    if let Some(root) = owned.iter().find(|root| paths_overlap(candidate, root)) {
        return Err(OverlapError::Cutex(root.clone()));
    }
    if let Some((id, _)) = others.iter().find(|(_, path)| paths_overlap(candidate, path)) {
        return Err(OverlapError::Source(id.clone()));
    }
    Ok(())
}

// ------------------------------------------------------------------ config

static CONFIG_LOCK: Mutex<()> = Mutex::new(());

pub fn configured_sources() -> Vec<ExtraUsageSource> {
    crate::config::store::load_codez_config().usage.extra_sources
}

/// Ids of configured but disabled sources (their rows are not counted).
pub fn disabled_source_ids() -> HashSet<String> {
    configured_sources()
        .into_iter()
        .filter(|entry| !entry.enabled)
        .map(|entry| entry.id)
        .collect()
}

/// Labels for `GET /v2/usage` (groupBy `source`).
pub fn source_labels() -> std::collections::HashMap<String, super::query::UsageSourceLabel> {
    configured_sources()
        .iter()
        .filter_map(SourceSpec::from_config)
        .map(|spec| {
            (
                spec.id.clone(),
                super::query::UsageSourceLabel {
                    label: spec.label.clone(),
                    kind: spec.kind.key().to_string(),
                    billing: spec.billing.key().to_string(),
                },
            )
        })
        .collect()
}

/// Read-modify-write of `usage.extraSources` under one process lock.
pub fn update_sources<T>(
    change: impl FnOnce(&mut Vec<ExtraUsageSource>) -> Result<T, SourceError>,
) -> Result<T, SourceError> {
    let _guard = CONFIG_LOCK.lock().map_err(|_| SourceError::Internal("config lock poisoned".into()))?;
    let mut config = crate::config::store::load_codez_config_checked()
        .map_err(|error| SourceError::Internal(format!("{error:#}")))?;
    let result = change(&mut config.usage.extra_sources)?;
    crate::config::store::save_codez_config(&config)
        .map_err(|error| SourceError::Internal(format!("{error:#}")))?;
    Ok(result)
}

#[derive(Debug)]
pub enum SourceError {
    Invalid(String),
    NotFound,
    OverlapsCutex(PathBuf),
    OverlapsSource(String),
    Conflict(String),
    Internal(String),
}

/// Body of `POST /v2/usage/sources`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NewSource {
    #[serde(default)]
    pub id: Option<String>,
    pub kind: String,
    pub path: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub billing: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// Body of `PATCH /v2/usage/sources/{id}`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SourcePatch {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub billing: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

fn guard(path: &Path, sources: &[ExtraUsageSource], except: Option<&str>) -> Result<(), SourceError> {
    let others = sources
        .iter()
        .filter(|entry| Some(entry.id.as_str()) != except)
        .map(|entry| (entry.id.clone(), PathBuf::from(&entry.path)))
        .collect::<Vec<_>>();
    match check_overlap(path, &cutex_owned_roots(), &others) {
        Ok(()) => Ok(()),
        Err(OverlapError::Cutex(root)) => Err(SourceError::OverlapsCutex(root)),
        Err(OverlapError::Source(id)) => Err(SourceError::OverlapsSource(id)),
    }
}

fn new_id(kind: SourceKind, taken: &[ExtraUsageSource]) -> String {
    let base = kind.key();
    if !taken.iter().any(|entry| entry.id == base) {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|id| !taken.iter().any(|entry| &entry.id == id))
        .expect("unbounded range")
}

pub fn add_source(request: NewSource) -> Result<SourceSpec, SourceError> {
    let kind = SourceKind::parse(&request.kind)
        .ok_or_else(|| SourceError::Invalid("kind must be claude-code, codex or zcode".into()))?;
    let billing = match request.billing.as_deref() {
        None => Billing::Subscription,
        Some(value) => Billing::parse(value)
            .ok_or_else(|| SourceError::Invalid("billing must be subscription or api".into()))?,
    };
    let label = match request.label.as_deref() {
        None | Some("") => kind.display().to_string(),
        Some(value) => validate_label(value).map_err(SourceError::Invalid)?,
    };
    if let Some(id) = &request.id {
        validate_id(id).map_err(SourceError::Invalid)?;
    }
    let path = normalize_source_path(&request.path).map_err(SourceError::Invalid)?;
    update_sources(|sources| {
        if sources.len() >= MAX_SOURCES {
            return Err(SourceError::Invalid(format!("at most {MAX_SOURCES} extra sources")));
        }
        guard(&path, sources, None)?;
        let id = match &request.id {
            Some(id) if sources.iter().any(|entry| &entry.id == id) => {
                return Err(SourceError::Conflict(format!("a source with id {id} exists")))
            }
            Some(id) => id.clone(),
            None => new_id(kind, sources),
        };
        let spec = SourceSpec {
            id,
            kind,
            path: path.clone(),
            label,
            billing,
            enabled: request.enabled.unwrap_or(true),
        };
        sources.push(spec.to_config());
        Ok(spec)
    })
}

pub fn patch_source(id: &str, patch: SourcePatch) -> Result<SourceSpec, SourceError> {
    let path = match patch.path.as_deref() {
        None => None,
        Some(raw) => Some(normalize_source_path(raw).map_err(SourceError::Invalid)?),
    };
    let label = match patch.label.as_deref() {
        None => None,
        Some(value) => Some(validate_label(value).map_err(SourceError::Invalid)?),
    };
    let billing = match patch.billing.as_deref() {
        None => None,
        Some(value) => Some(
            Billing::parse(value)
                .ok_or_else(|| SourceError::Invalid("billing must be subscription or api".into()))?,
        ),
    };
    update_sources(|sources| {
        let index = sources
            .iter()
            .position(|entry| entry.id == id)
            .ok_or(SourceError::NotFound)?;
        let mut spec = SourceSpec::from_config(&sources[index])
            .ok_or_else(|| SourceError::Invalid("this source was written by a newer cutex".into()))?;
        if let Some(path) = path {
            guard(&path, sources, Some(id))?;
            spec.path = path;
        }
        if let Some(label) = label {
            spec.label = label;
        }
        if let Some(billing) = billing {
            spec.billing = billing;
        }
        if let Some(enabled) = patch.enabled {
            spec.enabled = enabled;
        }
        sources[index] = spec.to_config();
        Ok(spec)
    })
}

/// Remove a source from the config; `purge` also deletes its rows.
pub fn remove_source(root: &Path, id: &str, purge: bool) -> Result<bool, SourceError> {
    update_sources(|sources| {
        let before = sources.len();
        sources.retain(|entry| entry.id != id);
        if sources.len() == before {
            return Err(SourceError::NotFound);
        }
        Ok(())
    })?;
    if purge {
        return store::purge_external_source(root, id).map_err(|error| SourceError::Internal(format!("{error:#}")));
    }
    Ok(false)
}

// ------------------------------------------------------------------ scan state

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct ScanState {
    #[serde(default)]
    version: u8,
    /// Source path the offsets belong to (a changed path starts over).
    #[serde(default)]
    path: String,
    #[serde(default)]
    files: BTreeMap<String, FileState>,
    #[serde(default)]
    last_scan_at: Option<String>,
    #[serde(default)]
    last_scan_ms: Option<u64>,
    /// Rows written by the last scan / since the state was created.
    #[serde(default)]
    last_rows: u64,
    #[serde(default)]
    rows: u64,
    #[serde(default)]
    last_errors: u64,
    #[serde(default)]
    errors: Vec<String>,
    /// ZCode: database watermark (older cutex builds ignore the field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    zcode: Option<zcode::ZcodeCursor>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct FileState {
    len: u64,
    #[serde(default)]
    mtime_ms: i64,
    #[serde(default)]
    inode: Option<u64>,
    /// Bytes consumed: everything before is turned into rows.
    offset: u64,
    #[serde(default)]
    cursor: Cursor,
}

/// Parsing context at `offset`.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct Cursor {
    /// Claude `promptId` / Codex `turn_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turn: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    /// Codex: the session belongs to cutex (originator).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    skip: bool,
    /// Codex: the rollout writes `token_usage_record` (token_count ignored).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    has_records: bool,
    /// Codex: cumulative total of the last counted `token_count`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_total: Option<u64>,
    /// Claude: id of the last emitted message (a stray repeat is ignored).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_message: Option<String>,
}

fn source_dir(root: &Path, id: &str) -> PathBuf {
    root.join(store::USAGE_EXTERNAL_DIR).join(id)
}

fn load_state(root: &Path, id: &str) -> ScanState {
    fs::read(source_dir(root, id).join(STATE_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<ScanState>(&bytes).ok())
        .filter(|state| state.version == STATE_VERSION)
        .unwrap_or_default()
}

fn save_state(root: &Path, id: &str, state: &ScanState) -> anyhow::Result<()> {
    let dir = source_dir(root, id);
    fs::create_dir_all(&dir)?;
    store::secure_directory(&root.join(store::USAGE_EXTERNAL_DIR))?;
    store::secure_directory(&dir)?;
    crate::config::atomic::write_private_pretty_json_atomic(&dir.join(STATE_FILE), state, "usage source scan state")
}

/// `status` object of a source resource.
pub fn status_json(root: &Path, id: &str) -> Value {
    let state = load_state(root, id);
    json!({
        "lastScanAt": state.last_scan_at,
        "lastScanMs": state.last_scan_ms,
        "files": state.files.len(),
        "rows": state.rows,
        "lastRows": state.last_rows,
        "errors": state.last_errors,
        "lastError": state.errors.last(),
        "scanning": is_scanning(root, id),
    })
}

pub fn source_json(root: &Path, entry: &ExtraUsageSource) -> Value {
    let spec = SourceSpec::from_config(entry);
    json!({
        "id": entry.id,
        "kind": entry.kind,
        "path": entry.path,
        "label": spec.as_ref().map(|spec| spec.label.clone()).unwrap_or_else(|| entry.label.clone()),
        "billing": spec.as_ref().map(|spec| spec.billing.key()).unwrap_or("subscription"),
        "enabled": entry.enabled,
        "supported": spec.is_some(),
        "status": status_json(root, &entry.id),
    })
}

/// Default directories offered for a new source (`~/.claude`, `~/.codex`, `~/.zcode`).
pub fn defaults_json() -> Value {
    let home = crate::config::paths::home_dir();
    Value::Array(
        SourceKind::ALL
            .iter()
            .map(|kind| {
                let path = home.as_ref().map(|home| home.join(kind.home_dir_name()));
                json!({
                    "kind": kind.key(),
                    "path": path.as_ref().map(|path| path.to_string_lossy().into_owned()),
                    "exists": path.as_ref().is_some_and(|path| path.is_dir()),
                })
            })
            .collect(),
    )
}

// ------------------------------------------------------------------ scanning

static RUNNING: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

fn running() -> &'static Mutex<HashSet<PathBuf>> {
    RUNNING.get_or_init(|| Mutex::new(HashSet::new()))
}

fn is_scanning(root: &Path, id: &str) -> bool {
    running()
        .lock()
        .map(|set| set.contains(&source_dir(root, id)))
        .unwrap_or(false)
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScanSummary {
    pub files: usize,
    pub rows: u64,
    pub errors: u64,
    /// Another scan of this source was already running.
    pub busy: bool,
}

/// Scan one source now (incremental). `root` is `runtime/management-v2`.
pub fn scan_source_at(
    root: &Path,
    spec: &SourceSpec,
    host_id: &str,
    now: DateTime<Utc>,
) -> anyhow::Result<ScanSummary> {
    let key = source_dir(root, &spec.id);
    {
        let mut set = running().lock().map_err(|_| anyhow::anyhow!("scan registry poisoned"))?;
        if !set.insert(key.clone()) {
            return Ok(ScanSummary { busy: true, ..ScanSummary::default() });
        }
    }
    let result = scan_locked(root, spec, host_id, now);
    if let Ok(mut set) = running().lock() {
        set.remove(&key);
    }
    result
}

fn scan_locked(
    root: &Path,
    spec: &SourceSpec,
    host_id: &str,
    now: DateTime<Utc>,
) -> anyhow::Result<ScanSummary> {
    let started = std::time::Instant::now();
    let mut state = load_state(root, &spec.id);
    let path_key = spec.path.to_string_lossy().into_owned();
    if state.version != STATE_VERSION || state.path != path_key {
        state = ScanState { version: STATE_VERSION, path: path_key, ..ScanState::default() };
    }
    let files = discover(spec.kind, &spec.path);
    let thread_ids = match spec.kind {
        SourceKind::Codex => store::ledger_thread_ids(root).unwrap_or_default(),
        SourceKind::ClaudeCode | SourceKind::Zcode => HashSet::new(),
    };
    let context = RowContext { spec, host_id, now, thread_ids: &thread_ids };
    let mut errors = Vec::new();
    let mut rows = 0_u64;
    let mut seen = HashSet::new();
    if spec.kind == SourceKind::Zcode {
        rows = zcode::scan(root, &mut state, &context, &mut errors, &mut seen);
    }
    for (relative, absolute) in &files {
        seen.insert(relative.clone());
        let Ok(meta) = fs::metadata(absolute) else {
            continue;
        };
        let signature = file_signature(&meta);
        let previous = state.files.get(relative).cloned();
        let mut file_state = match previous {
            Some(previous)
                if previous.inode == signature.2 && previous.len <= signature.0 && previous.offset <= signature.0 =>
            {
                if previous.len == signature.0 && previous.mtime_ms == signature.1 && previous.offset == previous.len {
                    continue;
                }
                previous
            }
            // New, replaced or truncated: start over (rows dedup by entry id).
            _ => FileState::default(),
        };
        match scan_file(absolute, relative, &mut file_state, &context) {
            Ok(entries) => {
                if let Err(error) = store::append_external_entries(root, &spec.id, &entries) {
                    errors.push(format!("{relative}: ledger write failed: {error:#}"));
                    continue;
                }
                rows += entries.len() as u64;
                file_state.len = signature.0;
                file_state.mtime_ms = signature.1;
                file_state.inode = signature.2;
                state.files.insert(relative.clone(), file_state);
            }
            Err(error) => errors.push(format!("{relative}: {}", error.kind())),
        }
    }
    // Files that are gone keep no state (their rows stay).
    state.files.retain(|relative, _| seen.contains(relative));
    state.last_scan_at = Some(now.to_rfc3339());
    state.last_scan_ms = Some(started.elapsed().as_millis() as u64);
    state.last_rows = rows;
    state.rows = state.rows.saturating_add(rows);
    state.last_errors = errors.len() as u64;
    state.errors = errors.iter().rev().take(MAX_ERRORS_KEPT).rev().cloned().collect();
    save_state(root, &spec.id, &state)?;
    Ok(ScanSummary { files: state.files.len(), rows, errors: errors.len() as u64, busy: false })
}

fn file_signature(meta: &fs::Metadata) -> (u64, i64, Option<u64>) {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0);
    #[cfg(unix)]
    let inode = {
        use std::os::unix::fs::MetadataExt;
        Some(meta.ino())
    };
    #[cfg(not(unix))]
    let inode = None;
    (meta.len(), mtime, inode)
}

/// `(relative path with '/', absolute path)` of the files a source reads.
fn discover(kind: SourceKind, base: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    match kind {
        SourceKind::ClaudeCode => walk(base, "projects", 0, &mut out, &|name| name.ends_with(".jsonl")),
        SourceKind::Codex => {
            let rollout = |name: &str| name.starts_with("rollout-") && name.ends_with(".jsonl");
            walk(base, "sessions", 0, &mut out, &rollout);
            walk(base, "archived_sessions", MAX_WALK_DEPTH, &mut out, &rollout);
        }
        // One database, read by `zcode::scan` (no line files).
        SourceKind::Zcode => {}
    }
    out.sort();
    out
}

fn walk(
    base: &Path,
    relative: &str,
    depth: usize,
    out: &mut Vec<(String, PathBuf)>,
    wanted: &dyn Fn(&str) -> bool,
) {
    let dir = base.join(relative);
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        // Never follow links: a source reads only its own tree.
        let Ok(kind) = entry.file_type() else { continue };
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let child = format!("{relative}/{name}");
        if kind.is_dir() && depth < MAX_WALK_DEPTH {
            walk(base, &child, depth + 1, out, wanted);
        } else if kind.is_file() && wanted(&name) {
            out.push((child, entry.path()));
        }
    }
}

struct RowContext<'a> {
    spec: &'a SourceSpec,
    host_id: &'a str,
    now: DateTime<Utc>,
    thread_ids: &'a HashSet<String>,
}

impl RowContext<'_> {
    fn settled(&self, timestamp: &str) -> bool {
        parse_timestamp(timestamp)
            .map(|at| (self.now - at.with_timezone(&Utc)).num_seconds() >= SETTLE_SECS)
            .unwrap_or(true)
    }

    #[allow(clippy::too_many_arguments)]
    fn sample(
        &self,
        entry_key: String,
        observed_at: &str,
        project_key: &str,
        project: Option<&str>,
        thread: &str,
        turn: &str,
        attribution: UsageAttribution,
        tokens: TokenUsageBreakdown,
    ) -> Option<UsageLedgerEntry> {
        let observed = parse_timestamp(observed_at).ok()?;
        let shown = project.unwrap_or(project_key);
        // ZCode rows carry no working directory: one agent, no project.
        let (agent_name, project) = match self.spec.kind {
            SourceKind::Zcode => (self.spec.kind.display().to_string(), None),
            _ => (format!("{} · {shown}", self.spec.kind.display()), Some(shown.to_string())),
        };
        let event_id = format!("ext:{}", entry_key);
        let sample = UsageSample {
            entry_id: format!("usage:{event_id}"),
            event_id,
            observed_at: observed.to_rfc3339(),
            host_id: self.host_id.to_string(),
            cutex_session_id: format!("ext:{}:{project_key}", self.spec.id),
            thread_id: thread.to_string(),
            turn_id: turn.to_string(),
            runtime_generation: 1,
            profile: None,
            profile_id: None,
            profile_deleted: false,
            source: Some(UsageSampleSource {
                id: self.spec.id.clone(),
                kind: self.spec.kind.key().to_string(),
                label: Some(self.spec.label.clone()),
                billing: Some(self.spec.billing.key().to_string()),
                agent_name: Some(agent_name),
                project,
            }),
            attribution,
            token_source: UsageTokenSource::NativeLast,
            model_context_window: None,
            compaction_count: None,
            model_requests: None,
            cumulative_total: tokens.clone(),
            tokens,
        };
        let entry = UsageLedgerEntry::Usage(Box::new(sample));
        entry.validate().ok()?;
        Some(entry)
    }
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value.map(str::trim).filter(|value| !value.is_empty()).map(str::to_string)
}

fn base_name(path: &str) -> Option<String> {
    non_empty(path.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next())
}

/// Read complete lines from `state.offset`, returning the rows to append;
/// advances `state.offset` / `state.cursor` past everything settled.
fn scan_file(
    path: &Path,
    relative: &str,
    state: &mut FileState,
    context: &RowContext<'_>,
) -> std::io::Result<Vec<UsageLedgerEntry>> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(state.offset))?;
    let mut reader = BufReader::new(file);
    match context.spec.kind {
        SourceKind::ClaudeCode => scan_claude(&mut reader, relative, state, context),
        SourceKind::Codex => scan_codex(&mut reader, relative, state, context),
        SourceKind::Zcode => Ok(Vec::new()),
    }
}

/// Next complete line (with its start offset) or `None` at a partial / no line.
fn next_line(reader: &mut impl BufRead, position: &mut u64, buffer: &mut Vec<u8>) -> std::io::Result<Option<u64>> {
    buffer.clear();
    let read = reader.read_until(b'\n', buffer)?;
    if read == 0 || buffer.last() != Some(&b'\n') {
        return Ok(None);
    }
    let start = *position;
    *position += read as u64;
    Ok(Some(start))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

/// Value of the first `"key":"value"` (plain JSON string without escapes).
fn string_field(line: &[u8], key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let start = line.windows(needle.len()).position(|window| window == needle.as_bytes())? + needle.len();
    let end = line[start..].iter().position(|byte| *byte == b'"' || *byte == b'\\')?;
    non_empty(std::str::from_utf8(&line[start..start + end]).ok())
}

// ---------------------------------------------------------------- Claude Code

#[derive(Deserialize)]
struct ClaudeLine {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(rename = "sessionId", default)]
    session_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    effort: Option<Value>,
    #[serde(default)]
    message: Option<ClaudeMessage>,
}

#[derive(Deserialize)]
struct ClaudeMessage {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<ClaudeUsage>,
}

#[derive(Deserialize, Default)]
struct ClaudeUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

impl ClaudeUsage {
    /// Same mapping as the Claude adapter (`engine.py` `_totals`).
    fn breakdown(&self) -> TokenUsageBreakdown {
        let input = self
            .input_tokens
            .saturating_add(self.cache_creation_input_tokens)
            .saturating_add(self.cache_read_input_tokens);
        TokenUsageBreakdown {
            total_tokens: input.saturating_add(self.output_tokens),
            input_tokens: input,
            cached_input_tokens: self.cache_read_input_tokens,
            cache_write_input_tokens: self.cache_creation_input_tokens,
            output_tokens: self.output_tokens,
            reasoning_output_tokens: 0,
        }
    }
}

/// Lines of one `message.id` being collected.
struct ClaudeGroup {
    message_id: String,
    start: u64,
    cursor: Cursor,
    candidate: Option<UsageLedgerEntry>,
    last_timestamp: Option<String>,
}

fn scan_claude(
    reader: &mut impl BufRead,
    relative: &str,
    state: &mut FileState,
    context: &RowContext<'_>,
) -> std::io::Result<Vec<UsageLedgerEntry>> {
    // projects/<slug>/<session>.jsonl or projects/<slug>/<session>/subagents/agent-*.jsonl
    let slug = relative.split('/').nth(1).unwrap_or("unknown").to_string();
    let mut out = Vec::new();
    let mut position = state.offset;
    let mut buffer = Vec::new();
    let mut cursor = state.cursor.clone();
    let mut group: Option<ClaudeGroup> = None;
    let settle = |group: Option<ClaudeGroup>, cursor: &mut Cursor, out: &mut Vec<UsageLedgerEntry>| {
        if let Some(group) = group {
            if cursor.last_message.as_deref() != Some(group.message_id.as_str()) {
                out.extend(group.candidate);
                cursor.last_message = Some(group.message_id);
            }
        }
    };
    while let Some(start) = next_line(reader, &mut position, &mut buffer)? {
        if contains(&buffer, b"\"type\":\"assistant\"") {
            let Ok(line) = serde_json::from_slice::<ClaudeLine>(&buffer) else { continue };
            if line.kind.as_deref() == Some("user") {
                // A tool result quoting an assistant transcript.
                settle(group.take(), &mut cursor, &mut out);
                if let Some(prompt) = string_field(&buffer, "promptId") {
                    cursor.turn = Some(prompt);
                }
                continue;
            }
            if line.kind.as_deref() != Some("assistant") {
                continue;
            }
            let Some(message) = line.message else { continue };
            let Some(message_id) = non_empty(message.id.as_deref()) else { continue };
            if let Some(project) = line.cwd.as_deref().and_then(base_name) {
                cursor.project = Some(project);
            }
            if group.as_ref().is_some_and(|group| group.message_id != message_id) {
                settle(group.take(), &mut cursor, &mut out);
            }
            let current = group.get_or_insert_with(|| ClaudeGroup {
                message_id: message_id.clone(),
                start,
                cursor: cursor.clone(),
                candidate: None,
                last_timestamp: None,
            });
            current.last_timestamp = line.timestamp.clone();
            let tokens = message.usage.unwrap_or_default().breakdown();
            let model = non_empty(message.model.as_deref()).filter(|model| model != "<synthetic>");
            if tokens.detailed_is_zero() || model.is_none() {
                continue;
            }
            let (Some(timestamp), Some(session)) = (line.timestamp.as_deref(), non_empty(line.session_id.as_deref())) else {
                continue;
            };
            let attribution = UsageAttribution {
                provider: Some("anthropic".into()),
                model,
                request_model: None,
                reasoning_effort: non_empty(line.effort.as_ref().and_then(Value::as_str)),
                service_tier: None,
            };
            let turn = cursor.turn.clone().unwrap_or_else(|| message_id.clone());
            let candidate = context.sample(
                format!("claude:{message_id}"),
                timestamp,
                &slug,
                cursor.project.as_deref(),
                &session,
                &turn,
                attribution,
                tokens,
            );
            if candidate.is_some() {
                current.candidate = candidate;
            }
        } else if contains(&buffer, b"\"type\":\"user\"") {
            // A user line (prompt or tool result) ends the message before it.
            settle(group.take(), &mut cursor, &mut out);
            if let Some(prompt) = string_field(&buffer, "promptId") {
                cursor.turn = Some(prompt);
            }
        }
    }
    match group {
        Some(group) if group.last_timestamp.as_deref().is_some_and(|at| !context.settled(at)) => {
            // Still streaming: read this message again next time.
            state.offset = group.start;
            state.cursor = group.cursor;
        }
        group => {
            settle(group, &mut cursor, &mut out);
            state.offset = position;
            state.cursor = cursor;
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- Codex

#[derive(Deserialize)]
struct CodexLine {
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    payload: Value,
}

#[derive(Deserialize, Default)]
struct CodexUsage {
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    cached_input_tokens: i64,
    #[serde(default)]
    cache_write_input_tokens: i64,
    #[serde(default)]
    output_tokens: i64,
    #[serde(default)]
    reasoning_output_tokens: i64,
    #[serde(default)]
    total_tokens: i64,
}

impl CodexUsage {
    fn from(value: &Value) -> Option<Self> {
        serde_json::from_value(value.clone()).ok()
    }

    fn breakdown(&self) -> TokenUsageBreakdown {
        let clamp = |value: i64| value.max(0) as u64;
        TokenUsageBreakdown {
            total_tokens: clamp(self.total_tokens),
            input_tokens: clamp(self.input_tokens),
            cached_input_tokens: clamp(self.cached_input_tokens),
            cache_write_input_tokens: clamp(self.cache_write_input_tokens),
            output_tokens: clamp(self.output_tokens),
            reasoning_output_tokens: clamp(self.reasoning_output_tokens),
        }
    }
}

fn scan_codex(
    reader: &mut impl BufRead,
    relative: &str,
    state: &mut FileState,
    context: &RowContext<'_>,
) -> std::io::Result<Vec<UsageLedgerEntry>> {
    let stem = relative
        .rsplit('/')
        .next()
        .and_then(|name| name.strip_suffix(".jsonl"))
        .unwrap_or(relative)
        .to_string();
    let mut out = Vec::new();
    let mut position = state.offset;
    let mut buffer = Vec::new();
    let mut cursor = state.cursor.clone();
    let mut consumed = position;
    while next_line(reader, &mut position, &mut buffer)?.is_some() {
        let interesting = [
            b"\"type\":\"session_meta\"".as_slice(),
            b"\"type\":\"turn_context\"",
            b"\"type\":\"token_usage_record\"",
            b"\"type\":\"token_count\"",
        ]
        .iter()
        .any(|needle| contains(&buffer, needle));
        if !interesting {
            consumed = position;
            continue;
        }
        let Ok(line) = serde_json::from_slice::<CodexLine>(&buffer) else {
            consumed = position;
            continue;
        };
        let payload = &line.payload;
        let usage_line = match line.kind.as_str() {
            "token_usage_record" => true,
            "event_msg" => payload.get("type").and_then(Value::as_str) == Some("token_count"),
            _ => false,
        };
        // A fresh usage line waits: a cutex-run thread may not be in the
        // ledger yet, and the record of a request precedes its token_count.
        if usage_line && !line.timestamp.as_deref().is_some_and(|at| context.settled(at)) {
            break;
        }
        consumed = position;
        let str_of = |key: &str| non_empty(payload.get(key).and_then(Value::as_str));
        match line.kind.as_str() {
            "session_meta" => {
                cursor.session = str_of("id");
                cursor.provider = str_of("model_provider");
                cursor.project = str_of("cwd").as_deref().and_then(base_name);
                cursor.skip = str_of("originator").is_some_and(|origin| origin.to_ascii_lowercase().starts_with("cutex"));
                cursor.last_total = None;
            }
            "turn_context" => {
                if let Some(model) = str_of("model") {
                    cursor.model = Some(model);
                }
                cursor.effort = str_of("effort");
                if let Some(turn) = str_of("turn_id") {
                    cursor.turn = Some(turn);
                }
            }
            "token_usage_record" => {
                cursor.has_records = true;
                let Some(response) = str_of("response_id") else { continue };
                let Some(tokens) = payload.get("usage").and_then(CodexUsage::from).map(|usage| usage.breakdown()) else {
                    continue;
                };
                let thread = str_of("thread_id").or_else(|| cursor.session.clone()).unwrap_or_else(|| stem.clone());
                if cursor.skip || context.thread_ids.contains(&thread) || tokens.detailed_is_zero() {
                    continue;
                }
                let turn = str_of("turn_id").or_else(|| cursor.turn.clone()).unwrap_or_else(|| response.clone());
                out.extend(codex_row(context, &cursor, &line, format!("codex:{response}"), &thread, &turn, tokens));
            }
            _ if usage_line && !cursor.has_records => {
                let info = payload.get("info").unwrap_or(&Value::Null);
                let Some(total) = info.get("total_token_usage").and_then(CodexUsage::from) else { continue };
                let total = total.total_tokens.max(0) as u64;
                if cursor.last_total == Some(total) {
                    continue;
                }
                cursor.last_total = Some(total);
                let Some(tokens) = info.get("last_token_usage").and_then(CodexUsage::from).map(|usage| usage.breakdown()) else {
                    continue;
                };
                let thread = cursor.session.clone().unwrap_or_else(|| stem.clone());
                if cursor.skip || context.thread_ids.contains(&thread) || tokens.detailed_is_zero() {
                    continue;
                }
                let turn = cursor.turn.clone().unwrap_or_else(|| format!("{thread}:{total}"));
                out.extend(codex_row(context, &cursor, &line, format!("codex:{thread}:{total}"), &thread, &turn, tokens));
            }
            _ => {}
        }
    }
    state.offset = consumed;
    state.cursor = cursor;
    Ok(out)
}

fn codex_row(
    context: &RowContext<'_>,
    cursor: &Cursor,
    line: &CodexLine,
    entry_key: String,
    thread: &str,
    turn: &str,
    tokens: TokenUsageBreakdown,
) -> Option<UsageLedgerEntry> {
    let timestamp = line.timestamp.as_deref()?;
    let project_key = cursor.project.clone().unwrap_or_else(|| "unknown".into());
    let attribution = UsageAttribution {
        provider: Some(cursor.provider.clone().unwrap_or_else(|| "openai".into())),
        model: cursor.model.clone(),
        request_model: None,
        reasoning_effort: cursor.effort.clone(),
        service_tier: None,
    };
    context.sample(entry_key, timestamp, &project_key, None, thread, turn, attribution, tokens)
}

// ------------------------------------------------------------------ daemon

/// `runtime/management-v2` of the running daemon.
pub fn ledger_root() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::runtime_dir()?.join("management-v2"))
}

/// Scan every enabled source once (management serve's background thread).
pub fn scan_all_enabled() {
    let Ok(root) = ledger_root() else { return };
    let host = crate::platform::host::current_host_name();
    for spec in configured_sources().iter().filter_map(SourceSpec::from_config) {
        if !spec.enabled {
            continue;
        }
        match scan_source_at(&root, &spec, &host, Utc::now()) {
            Ok(summary) if summary.errors > 0 => {
                eprintln!("usage source {}: {} files, {} rows, {} errors", spec.id, summary.files, summary.rows, summary.errors);
            }
            Ok(_) => {}
            Err(error) => eprintln!("usage source {} scan failed: {error:#}", spec.id),
        }
    }
}

/// Scan one configured source in the background; `false` when it is
/// unknown, disabled or already being scanned.
pub fn request_scan(id: &str) -> anyhow::Result<bool> {
    let root = ledger_root()?;
    let Some(spec) = configured_sources()
        .iter()
        .filter_map(SourceSpec::from_config)
        .find(|spec| spec.id == id && spec.enabled)
    else {
        return Ok(false);
    };
    if is_scanning(&root, id) {
        return Ok(false);
    }
    std::thread::Builder::new()
        .name("cutex-usage-source-scan".into())
        .spawn(move || {
            let host = crate::platform::host::current_host_name();
            if let Err(error) = scan_source_at(&root, &spec, &host, Utc::now()) {
                eprintln!("usage source {} scan failed: {error:#}", spec.id);
            }
        })
        .context("failed to start the usage source scan")?;
    Ok(true)
}

/// Background scanner of management serve: first pass after 30 s, then
/// every [`SCAN_INTERVAL`]. Never part of a `GET /v2/usage` request.
pub fn start_scanner() {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("cutex-usage-sources".into())
        .spawn(|| {
            std::thread::sleep(FIRST_SCAN_DELAY);
            loop {
                scan_all_enabled();
                std::thread::sleep(SCAN_INTERVAL);
            }
        });
}

#[path = "external_zcode.rs"]
mod zcode;

#[cfg(test)]
#[path = "external_tests.rs"]
mod tests;
