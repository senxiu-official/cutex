//! Official model catalogs as an overlay on the live upstream catalog
//! (`profile-catalog-overlay`).
//!
//! Official profiles (ChatGPT login, `source: official`) have no catalog of
//! their own: their upstream is the native model list (`native_models`: the
//! shared native home's `models_cache.json`, else the selected CLI's bundled
//! list), complete with the rows the native hides from pickers: visibility is
//! a display property and never makes a model unavailable. Edits store only
//! this profile's differences in `<profile dir>/model-overlay.json`:
//!
//! - per-entry `overrides` of the editable `ModelInfo` keys
//!   ([`EDITABLE_KEYS`]; `null` = key removed),
//! - named configurations (`requestModel` = the upstream base model; every
//!   other field follows the live base row, then the entry's overrides),
//! - `hidden` upstream entries (removed from this profile's list).
//!
//! An edit patches the entry it targets and nothing else: a value the owner
//! set stays pinned even while upstream happens to equal it, other entries
//! are never re-derived against the current upstream. Only reset drops an
//! entry's overrides (the one-time migration below diffs a whole snapshot).
//!
//! The effective catalog = upstream rows with the overlay applied, then the
//! named configurations in overlay order. New upstream models therefore show up
//! without any edit. Entries whose model (or a configuration's base model)
//! is no longer upstream are kept in the overlay, left out of the effective
//! catalog and reported as `unavailableModels`; a profile whose selected model
//! is such an entry fails to launch until it selects another model or the base
//! model returns. Every named configuration keeps its ID reserved, available or
//! not: an upstream model with the same slug stays out of the effective catalog
//! until the owner deletes the configuration, and an existing ID's
//! `requestModel` never changes (reusing the ID needs a delete first).
//!
//! The native still reads one catalog file: cutex writes the effective catalog
//! to a content-addressed `models-<sha256>.json` (never rewritten) and points
//! the profile's `model_catalog_json` at it. This happens after every overlay
//! edit and again before each launch ([`refresh_for_launch`]), so a launch
//! review / receipt digests the exact file the runtime reads, as before. An
//! empty overlay removes the file pointer: the profile is back on the plain
//! native list. The overlay file itself stays (`entries: []`): it marks the
//! profile as migrated, so an emptied overlay can never be mistaken for a
//! legacy snapshot. Commit order: overlay first, then the pointer; a crash in
//! between leaves a pointer the next edit / launch refresh re-derives from the
//! overlay. A per-profile file lock (`model-overlay.lock`, also held across
//! processes, e.g. a standalone CLI launch next to the daemon) serializes
//! migration, reads, edits and pointer updates.
//!
//! One-time migration: an official profile that still points at a legacy
//! immutable snapshot (`models-<uuid>.json`, written by the first catalog edit
//! before overlays) and has no overlay is converted on first use: overlay =
//! the snapshot's editable differences against the current upstream (upstream
//! models missing from the snapshot are not hidden: they may be new). The
//! previous `config.toml` and snapshot are copied to
//! `<profile dir>/pre-overlay-<timestamp>/`; `migratedFrom` names the snapshot.
//! A content-addressed `models-<sha256>.json` pointer without an overlay is
//! what an older release left when it deleted the overlay of the last entry
//! before clearing the pointer: it gets the empty marker and the pointer is
//! cleared (never re-migrated).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{bail, ensure, Context};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::config::atomic::{write_private_bytes_atomic, write_private_pretty_json_atomic};
use crate::config::global_settings::ConfigValueUpdate;
use crate::profiles::catalog_api;
use crate::profiles::codex_profile::CodexProfileConfigPatch;
use crate::profiles::model::{CliKind, StoredAccount};
use crate::profiles::settings_api::{self, ProfileApiPatch};

pub const OVERLAY_FILE: &str = "model-overlay.json";
/// Per-profile lock file (exclusive OS file lock, cross-process).
pub const LOCK_FILE: &str = "model-overlay.lock";
pub const OVERLAY_VERSION: u32 = 1;
/// `ModelInfo` keys a profile may override (the Management model patch).
pub const EDITABLE_KEYS: &[&str] = &[
    "display_name",
    "context_window",
    "max_context_window",
    "auto_compact_token_limit",
    "effective_context_window_percent",
    "input_modalities",
    "supported_reasoning_levels",
    "default_reasoning_level",
];

/// Threads of one process queue here before taking the profile file lock.
static LOCK: Mutex<()> = Mutex::new(());

/// Held while an overlay, its effective file and the pointer are read or
/// written. Field order: the file lock is released before the mutex.
struct Guard {
    _file: std::fs::File,
    _mutex: std::sync::MutexGuard<'static, ()>,
}

fn lock(account: &StoredAccount) -> anyhow::Result<Guard> {
    let mutex = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = catalog_api::profile_dir(account)?;
    crate::profiles::materialize::create_private_dir_all(&dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join(LOCK_FILE))
        .context("model overlay lock could not be opened")?;
    file.lock_exclusive().context("model overlay lock")?;
    Ok(Guard { _file: file, _mutex: mutex })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Overlay {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migrated_from: Option<String>,
    #[serde(default)]
    pub entries: Vec<Entry>,
}

impl Default for Overlay {
    fn default() -> Self {
        Self { version: OVERLAY_VERSION, migrated_from: None, entries: Vec::new() }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Entry {
    pub id: String,
    /// Named configuration: the upstream base model sent to the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_model: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub overrides: Map<String, Value>,
}

/// Effective rows plus overlay entries that no longer resolve.
#[derive(Debug, Default)]
pub struct Resolved {
    pub rows: Vec<Value>,
    pub unavailable: Vec<Value>,
}

/// Which edit `edit` applies to its target entry.
pub enum Change<'a> {
    /// Field edits. `pinned` = the editable keys the patch sets explicitly:
    /// a changed value is stored even when it equals upstream right now.
    Patch { request_model: &'a ConfigValueUpdate<String>, pinned: &'a [&'static str] },
    /// Back to the live upstream row (a configuration keeps its binding).
    Reset,
    /// The entry leaves the list (a configuration is dropped, an upstream
    /// model hidden).
    Remove,
}

fn slug(row: &Value) -> Option<&str> {
    row.get("slug").and_then(Value::as_str)
}

fn is_config(row: &Value) -> bool {
    row.get("request_model").is_some_and(|value| !value.is_null())
}

fn upstream_base<'a>(upstream: &'a [Value], id: &str) -> Option<&'a Value> {
    upstream.iter().find(|row| slug(row) == Some(id) && !is_config(row))
}

/// Whether `id` names one of the overlay's configurations (available or not).
fn reserved(overlay: &Overlay, id: &str) -> bool {
    overlay.entries.iter().any(|entry| entry.id == id && entry.request_model.is_some())
}

/// The upstream base a configuration targets. A reserved configuration ID
/// is never a base, even when upstream has that slug.
fn base_of<'a>(upstream: &'a [Value], overlay: &Overlay, target: &str) -> Option<&'a Value> {
    (!reserved(overlay, target)).then(|| upstream_base(upstream, target)).flatten()
}

fn apply_overrides(row: &mut Value, overrides: &Map<String, Value>) {
    let Some(object) = row.as_object_mut() else { return };
    for (key, value) in overrides {
        if value.is_null() {
            object.remove(key);
        } else {
            object.insert(key.clone(), value.clone());
        }
    }
}

/// A new named configuration before its own overrides (same as
/// `model_variants::patch_row` creates it).
pub fn config_row(base: &Value, id: &str, request_model: &str) -> Value {
    let mut row = base.clone();
    row["slug"] = json!(id);
    row["display_name"] = json!(id);
    row["request_model"] = json!(request_model);
    row["visibility"] = json!("list");
    row
}

fn present(value: Option<&Value>) -> Option<&Value> {
    value.filter(|value| !value.is_null())
}

fn editable_diff(reference: &Value, row: &Value) -> Map<String, Value> {
    let mut overrides = Map::new();
    for key in EDITABLE_KEYS {
        let after = present(row.get(*key));
        if present(reference.get(*key)) != after {
            overrides.insert((*key).to_string(), after.cloned().unwrap_or(Value::Null));
        }
    }
    overrides
}

fn editable_values(row: &Value) -> Map<String, Value> {
    EDITABLE_KEYS
        .iter()
        .filter_map(|key| present(row.get(*key)).map(|value| ((*key).to_string(), value.clone())))
        .collect()
}

/// Whether an entry of `overlay` no longer resolves against `upstream`.
fn orphaned(entry: &Entry, upstream: &[Value], overlay: &Overlay) -> bool {
    match &entry.request_model {
        Some(target) => base_of(upstream, overlay, target).is_none(),
        None => upstream_base(upstream, &entry.id).is_none(),
    }
}

/// Upstream rows with the overlay applied, then named configurations (pure).
pub fn resolve(upstream: &[Value], overlay: &Overlay) -> Resolved {
    let mut rows = Vec::new();
    for upstream_row in upstream {
        let Some(id) = slug(upstream_row) else { continue };
        // A configuration keeps its ID even if upstream later adds that slug,
        // also while the configuration itself is unavailable.
        if reserved(overlay, id) {
            continue;
        }
        let entry = overlay.entries.iter().find(|entry| entry.id == id && entry.request_model.is_none());
        if entry.is_some_and(|entry| entry.hidden) {
            continue;
        }
        let mut row = upstream_row.clone();
        if let Some(entry) = entry {
            apply_overrides(&mut row, &entry.overrides);
        }
        rows.push(row);
    }
    let mut unavailable = Vec::new();
    for entry in &overlay.entries {
        match &entry.request_model {
            Some(target) => match base_of(upstream, overlay, target) {
                Some(base) => {
                    let mut row = config_row(base, &entry.id, target);
                    apply_overrides(&mut row, &entry.overrides);
                    rows.push(row);
                }
                None => unavailable.push(json!({
                    "id": entry.id,
                    "requestModel": target,
                    "displayName": entry.overrides.get("display_name").filter(|v| !v.is_null()),
                    "reason": "baseModelMissing",
                })),
            },
            None if !entry.hidden && orphaned(entry, upstream, overlay) => unavailable.push(json!({
                "id": entry.id,
                "requestModel": Value::Null,
                "displayName": entry.overrides.get("display_name").filter(|v| !v.is_null()),
                "reason": "modelMissing",
            })),
            None => {}
        }
    }
    Resolved { rows, unavailable }
}

/// The overlay that turns `upstream` into `edited` (pure; the one-time
/// migration of a whole snapshot, edits use [`edit`]). With `hide_missing`,
/// upstream rows absent from `edited` become hidden entries; migration passes
/// false. Entries of `previous` that no longer resolve are carried over
/// unchanged.
pub fn diff(upstream: &[Value], edited: &[Value], previous: &Overlay, hide_missing: bool) -> Overlay {
    let mut entries: Vec<Entry> = Vec::new();
    for row in edited {
        let Some(id) = slug(row) else { continue };
        match row.get("request_model").and_then(Value::as_str) {
            Some(target) => {
                let overrides = match upstream_base(upstream, target) {
                    Some(base) => editable_diff(&config_row(base, id, target), row),
                    None => editable_values(row),
                };
                entries.push(Entry { id: id.into(), request_model: Some(target.into()), hidden: false, overrides });
            }
            None => {
                let overrides = match upstream_base(upstream, id) {
                    Some(base) => editable_diff(base, row),
                    None => editable_values(row),
                };
                if !overrides.is_empty() {
                    entries.push(Entry { id: id.into(), request_model: None, hidden: false, overrides });
                }
            }
        }
    }
    if hide_missing {
        for row in upstream {
            let Some(id) = slug(row) else { continue };
            if !edited.iter().any(|edited| slug(edited) == Some(id)) && !entries.iter().any(|entry| entry.id == id) {
                entries.push(Entry { id: id.into(), hidden: true, ..Entry::default() });
            }
        }
    }
    for entry in &previous.entries {
        if orphaned(entry, upstream, previous) && !entries.iter().any(|kept| kept.id == entry.id) {
            entries.push(entry.clone());
        }
    }
    Overlay { version: OVERLAY_VERSION, migrated_from: previous.migrated_from.clone(), entries }
}

/// The stored entry for `id` after one edit (pure). `before` / `after` are
/// the target's effective row around the edit (`None` = not listed).
fn edited_entry(
    upstream: &[Value],
    previous: &Overlay,
    id: &str,
    before: Option<&Value>,
    after: Option<&Value>,
    change: &Change<'_>,
) -> anyhow::Result<Option<Entry>> {
    let Some(row) = after else {
        // Removed: a configuration is dropped, an upstream model hidden.
        let was_config = before.is_some_and(is_config);
        return Ok((!was_config && upstream_base(upstream, id).is_some())
            .then(|| Entry { id: id.into(), hidden: true, ..Entry::default() }));
    };
    let request_model = row.get("request_model").and_then(Value::as_str);
    let reference = match request_model {
        Some(target) => config_row(
            base_of(upstream, previous, target).context("requestModel must reference a known base model")?,
            id,
            target,
        ),
        None => upstream_base(upstream, id).cloned().with_context(|| format!("model {id} is not upstream"))?,
    };
    let stored = previous.entries.iter().find(|entry| entry.id == id && !entry.hidden);
    let overrides = match (change, stored, before) {
        (Change::Patch { pinned, .. }, Some(stored), Some(before)) => {
            // Patch the stored entry: untouched keys keep their overrides,
            // pinned ones too while upstream happens to equal them.
            let mut overrides = stored.overrides.clone();
            for key in EDITABLE_KEYS {
                let value = present(row.get(*key));
                if present(before.get(*key)) == value {
                    continue;
                }
                if pinned.contains(key) || present(reference.get(*key)) != value {
                    overrides.insert((*key).to_string(), value.cloned().unwrap_or(Value::Null));
                } else {
                    overrides.remove(*key);
                }
            }
            overrides
        }
        (Change::Patch { pinned, .. }, _, _) => {
            // A new entry (or one shown again): its differences plus every
            // value the patch set.
            let mut overrides = editable_diff(&reference, row);
            for key in pinned.iter().filter(|key| EDITABLE_KEYS.contains(key)) {
                overrides.insert((*key).to_string(), present(row.get(*key)).cloned().unwrap_or(Value::Null));
            }
            overrides
        }
        (Change::Reset | Change::Remove, _, _) => editable_diff(&reference, row),
    };
    if request_model.is_none() && overrides.is_empty() {
        return Ok(None);
    }
    Ok(Some(Entry { id: id.into(), request_model: request_model.map(Into::into), hidden: false, overrides }))
}

/// Official Codex profiles use an overlay; API-key custom profiles keep their
/// own discovered catalog files.
pub fn uses_overlay(account: &StoredAccount) -> bool {
    account.cli_kind == CliKind::Codex && !catalog_api::discovery_supported(account)
}

fn overlay_path(account: &StoredAccount) -> anyhow::Result<PathBuf> {
    Ok(catalog_api::profile_dir(account)?.join(OVERLAY_FILE))
}

pub fn load(account: &StoredAccount) -> anyhow::Result<Option<Overlay>> {
    let path = overlay_path(account)?;
    match std::fs::read(&path) {
        Ok(bytes) => {
            let overlay: Overlay = serde_json::from_slice(&bytes)
                .with_context(|| format!("model overlay is invalid: {}", path.display()))?;
            ensure!(overlay.version == OVERLAY_VERSION, "unsupported model overlay version {}", overlay.version);
            Ok(Some(overlay))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("model overlay could not be read"),
    }
}

/// The overlay is the source of truth and is written first; an empty one
/// stays as the "migrated" marker.
fn save(account: &StoredAccount, overlay: &Overlay) -> anyhow::Result<()> {
    write_private_pretty_json_atomic(&overlay_path(account)?, overlay, "profile model overlay")
}

/// The live upstream rows, hidden ones included.
pub fn upstream_rows(account: &StoredAccount) -> anyhow::Result<Vec<Value>> {
    crate::profiles::native_models::upstream_for_profile(account)
        .map(|models| models.rows)
        .map_err(anyhow::Error::msg)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

fn set_pointer(account: &StoredAccount, pointer: ConfigValueUpdate<String>) -> anyhow::Result<()> {
    let patch = ProfileApiPatch {
        codex_config: CodexProfileConfigPatch { model_catalog_json: pointer, ..CodexProfileConfigPatch::default() },
        ..ProfileApiPatch::default()
    };
    settings_api::update_profile(&account.id, &patch).map(|_| ())
}

/// Write the effective catalog as `models-<sha256>.json` (content-addressed,
/// never rewritten) unless it exists with that content.
pub fn write_effective_file(dir: &Path, catalog: &Value) -> anyhow::Result<PathBuf> {
    let mut bytes = serde_json::to_vec_pretty(catalog)?;
    bytes.push(b'\n');
    let path = dir.join(format!("models-{}.json", sha256_hex(&bytes)));
    let intact = std::fs::read(&path).is_ok_and(|existing| existing == bytes);
    if !intact {
        write_private_bytes_atomic(&path, &bytes)?;
    }
    Ok(path.canonicalize()?)
}

/// Point the profile at the effective catalog of `overlay` (or at nothing for
/// an empty overlay). Returns the selected file.
fn materialize(account: &StoredAccount, overlay: &Overlay, upstream: &[Value]) -> anyhow::Result<Option<PathBuf>> {
    let current = settings_api::codex_snapshot(account).model_catalog_json;
    if overlay.entries.is_empty() {
        if current.is_some() {
            set_pointer(account, ConfigValueUpdate::Clear)?;
        }
        return Ok(None);
    }
    let catalog = json!({ "models": resolve(upstream, overlay).rows });
    catalog_api::validate_catalog(&catalog)?;
    let path = write_effective_file(&catalog_api::profile_dir(account)?, &catalog)?;
    let pointer = path.to_string_lossy().into_owned();
    if current.as_deref() != Some(pointer.as_str()) {
        set_pointer(account, ConfigValueUpdate::Set(pointer))?;
    }
    Ok(Some(path))
}

fn catalog_stem(path: &Path) -> Option<&str> {
    path.file_name()?.to_str()?.strip_prefix("models-")?.strip_suffix(".json")
}

/// `models-<uuid>.json`: a pre-overlay snapshot (a hand-written local
/// `models.json` is never converted).
fn legacy_snapshot(path: &Path) -> bool {
    catalog_stem(path).is_some_and(|stem| uuid::Uuid::parse_str(stem).is_ok())
}

/// `models-<sha256>.json`: an effective file derived from an overlay.
fn effective_file(path: &Path) -> bool {
    catalog_stem(path).is_some_and(|stem| stem.len() == 64 && stem.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// One-time conversion of a legacy official snapshot into an overlay. A no-op
/// for API-key profiles, profiles with an overlay, or without a catalog. When
/// upstream is unavailable it leaves the snapshot in place and retries later.
fn migrate_locked(account: &StoredAccount) -> anyhow::Result<bool> {
    if !uses_overlay(account) || load(account)?.is_some() {
        return Ok(false);
    }
    let state = catalog_api::load(account)?;
    let Some(path) = state.path.as_ref() else { return Ok(false) };
    if effective_file(path) {
        // An older release deleted the overlay of the last entry before it
        // cleared the pointer: the edits were removed on purpose.
        save(account, &Overlay::default())?;
        set_pointer(account, ConfigValueUpdate::Clear)?;
        eprintln!(
            "cutex-catalog-overlay: profile {} pointed at {} without an overlay; marked as emptied, pointer cleared",
            account.id,
            path.display()
        );
        return Ok(true);
    }
    if !legacy_snapshot(path) {
        return Ok(false);
    }
    let Some(catalog) = state.catalog.as_ref() else { return Ok(false) };
    let Ok(upstream) = upstream_rows(account) else { return Ok(false) };
    let rows = catalog.get("models").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut overlay = diff(&upstream, &rows, &Overlay::default(), false);
    overlay.migrated_from = Some(path.to_string_lossy().into_owned());
    // Backup: the previous profile config and the snapshot it selected.
    let dir = catalog_api::profile_dir(account)?;
    let backup = dir.join(format!("pre-overlay-{}", chrono::Utc::now().format("%Y%m%dT%H%M%SZ")));
    crate::profiles::materialize::create_private_dir_all(&backup)?;
    let files = crate::profiles::materialize::materialized_account_files(account)?;
    if files.config_path.exists() {
        std::fs::copy(&files.config_path, backup.join("config.toml"))?;
    }
    if let Some(name) = path.file_name() {
        std::fs::copy(path, backup.join(name))?;
    }
    save(account, &overlay)?;
    materialize(account, &overlay, &upstream)?;
    eprintln!(
        "cutex-catalog-overlay: profile {} migrated from {} ({} overlay entries; backup {})",
        account.id,
        path.display(),
        overlay.entries.len(),
        backup.display()
    );
    Ok(true)
}

pub fn ensure_migrated(account: &StoredAccount) -> anyhow::Result<bool> {
    let _guard = lock(account)?;
    migrate_locked(account)
}

/// The profile's overlay for display, migrated first (a failed migration is
/// logged and retried later).
pub fn current(account: &StoredAccount) -> anyhow::Result<Option<Overlay>> {
    let _guard = lock(account)?;
    if let Err(error) = migrate_locked(account) {
        eprintln!("cutex-catalog-overlay: migration of {} skipped: {error:#}", account.id);
    }
    load(account)
}

/// Apply one edit of entry `id` to the effective rows and store it in the
/// overlay; every other entry is kept as stored.
pub fn edit<T>(
    account: &StoredAccount,
    id: &str,
    change: Change<'_>,
    apply: impl FnOnce(&mut Value, &[Value]) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let _guard = lock(account)?;
    migrate_locked(account)?;
    let upstream = upstream_rows(account)?;
    let previous = load(account)?.unwrap_or_default();
    // The stored entry decides, not the resolved rows: an unavailable entry
    // keeps its binding and its ID.
    if let Some(stored) = previous.entries.iter().find(|entry| entry.id == id) {
        if let Change::Patch { request_model, .. } = &change {
            let target = match request_model {
                ConfigValueUpdate::Set(target) => Some(target.as_str()),
                ConfigValueUpdate::Clear => None,
                ConfigValueUpdate::Unchanged => stored.request_model.as_deref(),
            };
            ensure!(
                target == stored.request_model.as_deref(),
                "requestModel cannot change for an existing model ID; delete {id} first to reuse the ID"
            );
        }
        if !stored.hidden && orphaned(stored, &upstream, &previous) {
            match &stored.request_model {
                Some(target) => bail!(
                    "model configuration {id} is unavailable (its base model {target} is no longer upstream); \
                     delete it to reuse the ID, or wait for the base model to return"
                ),
                None => bail!("model {id} is no longer upstream; delete its entry"),
            }
        }
    }
    let mut catalog = json!({ "models": resolve(&upstream, &previous).rows });
    let find = |catalog: &Value| {
        catalog.get("models").and_then(Value::as_array)
            .and_then(|rows| rows.iter().find(|row| slug(row) == Some(id)).cloned())
    };
    let before = find(&catalog);
    let result = apply(&mut catalog, &upstream)?;
    let after = find(&catalog);
    let entry = edited_entry(&upstream, &previous, id, before.as_ref(), after.as_ref(), &change)?;
    let mut next = previous.clone();
    match (next.entries.iter().position(|stored| stored.id == id), entry) {
        (Some(index), Some(entry)) => next.entries[index] = entry,
        (Some(index), None) => {
            next.entries.remove(index);
        }
        (None, Some(entry)) => next.entries.push(entry),
        (None, None) => {}
    }
    // Validate before anything is written.
    catalog_api::validate_catalog(&json!({ "models": resolve(&upstream, &next).rows }))?;
    save(account, &next)?;
    materialize(account, &next, &upstream)?;
    Ok(result)
}

/// Drop an overlay entry that no longer resolves (its model left upstream).
/// Returns false when `id` is not such an entry.
pub fn remove_unavailable(account: &StoredAccount, id: &str) -> anyhow::Result<bool> {
    let _guard = lock(account)?;
    migrate_locked(account)?;
    let Some(mut overlay) = load(account)? else { return Ok(false) };
    let upstream = upstream_rows(account)?;
    let Some(index) = overlay.entries.iter().position(|entry| entry.id == id) else { return Ok(false) };
    let entry = &overlay.entries[index];
    if entry.hidden || !orphaned(entry, &upstream, &overlay) {
        return Ok(false);
    }
    overlay.entries.remove(index);
    save(account, &overlay)?;
    materialize(account, &overlay, &upstream)?;
    Ok(true)
}

/// Before a launch reviews the profile: migrate a legacy snapshot and point
/// the profile at the current effective catalog (new upstream models). Best
/// effort: on failure the previously selected file stays, and the launch
/// review digests whatever file the profile names.
pub fn refresh_for_launch(profile: &str) {
    let result = (|| -> anyhow::Result<()> {
        let store = settings_api::load_store()?;
        let Some(account) = settings_api::find(&store, profile) else { return Ok(()) };
        if !uses_overlay(account) {
            return Ok(());
        }
        let _guard = lock(account)?;
        migrate_locked(account)?;
        let Some(overlay) = load(account)? else { return Ok(()) };
        let upstream = if overlay.entries.is_empty() { Vec::new() } else { upstream_rows(account)? };
        materialize(account, &overlay, &upstream)?;
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!("cutex-catalog-overlay: refresh for {profile} skipped: {error:#}");
    }
}

#[cfg(test)]
#[path = "catalog_overlay_tests.rs"]
mod tests;
