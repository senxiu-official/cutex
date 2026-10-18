//! Profile lifecycle for Management clients: create, duplicate, rename and
//! delete (`/v2/profiles`).
//!
//! Layout is the one `cutex login` / `cutex profile copy` produce: a
//! `StoredAccount` in the profile store plus materialized files under
//! `<config dir>/profiles/<id>/` (Codex `auth.json`, `config.toml`, optional
//! `models.json`, status items; Claude `claude/`). Differences from the CLI
//! are deliberate and documented on each function: a created profile may
//! start without credentials (log in / set the API key afterwards), a
//! duplicate never copies credentials, delete moves the profile directory to
//! `<config dir>/deleted-profiles/<timestamp>-<id>/` instead of leaving it
//! behind, and rename works while runtimes launched on the profile run (they
//! are related to it by id; docs/profile-id-and-runtimes.md).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;
use serde_json::json;

use crate::config::atomic::write_private_pretty_json_atomic;
use crate::config::store::{load_codez_config_checked, load_quick_state, save_codez_config, save_quick_state};
use crate::config::text::{read_optional_text, write_optional_text_if_changed};
use crate::profiles::appearance::{ProfileOptions, Provider};
use crate::profiles::materialize::{
    ensure_materialized_account_dir, materialize_new_account_files, materialized_account_files,
    materialized_profiles_dir, refresh_materialized_account_config,
};
use crate::profiles::model::{AccountsStore, CliKind, RuntimeConfig, StoredAccount};
use crate::profiles::profile_config::{parse_toml_table, profile_uses_local_model_catalog};
use crate::profiles::providers::{api_key_profile_config, find_preset, ApiKeyPreset};
use crate::profiles::references::{
    remove_global_profile_references, remove_profile_references, rename_all_profile_references,
    ProfileReferenceChanges,
};
use crate::profiles::settings_api::{find, load_store, ApiKey};
use crate::profiles::store::save_store;
use crate::session::model::{CutexSessionRecord, CutexSessionStore};

pub const MAX_PROFILE_NAME_CHARS: usize = 64;
pub const DELETED_PROFILES_DIR: &str = "deleted-profiles";
const SESSION_STORE_RETRIES: usize = 6;

/// Serializes Management profile lifecycle writes within the daemon.
static LIFECYCLE_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn lock() -> std::sync::MutexGuard<'static, ()> {
    LIFECYCLE_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewProfileKind {
    /// Codex profile for a ChatGPT account login (`source: official`).
    Chatgpt,
    /// Claude Code profile (shared official runtime).
    Claude,
    /// Codex profile on a third-party / OpenAI API-key provider.
    ApiKey,
}

impl NewProfileKind {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "chatgpt" => Some(Self::Chatgpt),
            "claude" => Some(Self::Claude),
            "apiKey" => Some(Self::ApiKey),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chatgpt => "chatgpt",
            Self::Claude => "claude",
            Self::ApiKey => "apiKey",
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewProfile {
    pub name: String,
    pub kind: NewProfileKind,
    /// API-key preset (`None` = `custom`). Only for `ApiKey`.
    pub preset: Option<&'static ApiKeyPreset>,
    /// Overrides the preset base URL. Only for `ApiKey`.
    pub base_url: Option<String>,
    pub model: Option<String>,
    /// Optional API key written with the profile. Only for `ApiKey`.
    pub api_key: Option<ApiKey>,
}

/// A session that references a profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRef {
    pub cutex_session_id: String,
    pub display_name: String,
    /// `active` | `retired`.
    pub lifecycle: String,
    /// The session is configured to use the profile.
    pub configured: bool,
    /// A runtime currently bound to the session was launched on the profile.
    pub launched: bool,
}

#[derive(Debug)]
pub enum LifecycleError {
    Invalid(String),
    NotFound,
    NameConflict(String),
    InUse(Vec<SessionRef>),
    IsDefault(String),
    Store(anyhow::Error),
}

impl std::fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) => f.write_str(message),
            Self::NotFound => f.write_str("no profile has this ID or name"),
            Self::NameConflict(name) => write!(f, "a profile named {name} already exists"),
            Self::InUse(sessions) => write!(f, "{} session(s) still reference this profile", sessions.len()),
            Self::IsDefault(name) => write!(f, "{name} is the default profile; choose another default first"),
            Self::Store(error) => write!(f, "{error:#}"),
        }
    }
}

impl From<anyhow::Error> for LifecycleError {
    fn from(error: anyhow::Error) -> Self {
        Self::Store(error)
    }
}

/// New profile names: 1-64 visible characters, no surrounding whitespace,
/// no control characters, no `/` or `\` (names are addressable as one
/// `/v2/profiles/{profile}` path segment).
pub fn validate_profile_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed.chars().count() > MAX_PROFILE_NAME_CHARS
        || trimmed.chars().any(|c| c.is_control() || c == '/' || c == '\\')
    {
        return Err(format!(
            "name must be 1-{MAX_PROFILE_NAME_CHARS} visible characters without '/', '\\' or control characters"
        ));
    }
    Ok(trimmed.to_string())
}

/// The name must not equal another profile's name or ID (lookups accept both).
pub(crate) fn ensure_available(store: &AccountsStore, name: &str, ignore_id: Option<&str>) -> Result<(), LifecycleError> {
    let taken = store
        .accounts
        .iter()
        .filter(|account| Some(account.id.as_str()) != ignore_id)
        .any(|account| account.name == name || account.id == name);
    if taken {
        Err(LifecycleError::NameConflict(name.to_string()))
    } else {
        Ok(())
    }
}

pub(crate) fn base_account(name: &str, cli_kind: CliKind, source: &str) -> StoredAccount {
    StoredAccount {
        options: Default::default(),
        browser_tools: false,
        search_tool: false,
        quota_reader: None,
        private_chatgpt_auth_file: false,
        // New Claude profiles read the workspace's Claude Code project settings.
        claude_code: crate::profiles::claude_code::ClaudeCodeOptions::for_new_profile(&cli_kind),
        id: uuid::Uuid::new_v4().to_string(),
        name: name.to_string(),
        email: None,
        plan_type: None,
        plan_label: None,
        quota_preset: Default::default(),
        source: Some(source.to_string()),
        runtime: RuntimeConfig::Host,
        proxy: None,
        session: None,
        cli_kind,
        default_cli_args: Vec::new(),
        agent_name: None,
        last_used_at: None,
        provider_category: None,
        claude_account: None,
        model_display_names: Default::default(),
    }
}

/// The Claude Code profile `cutex login --cli claude` creates.
pub fn new_claude_account(name: &str) -> StoredAccount {
    let mut account = base_account(name, CliKind::Claude, "official");
    account.options = ProfileOptions { provider: Some(Provider::Claude), ..Default::default() };
    account
}

pub(crate) fn profile_dir(account: &StoredAccount) -> anyhow::Result<PathBuf> {
    Ok(materialized_profiles_dir()?.join(&account.id))
}

pub(crate) fn remove_profile_dir(account: &StoredAccount) {
    if let Ok(dir) = profile_dir(account) {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Create a profile.
///
/// - `chatgpt`: Codex `source: official` profile without credentials; log in
///   with `POST /v2/profiles/{p}/login {type:"chatgpt"}`.
/// - `apiKey`: Codex `source: api-key` profile with the preset's (or custom)
///   `[model_providers.<id>]` (same layout as `cutex login --api-key`);
///   without `api_key` it has no auth.json until the key is set.
/// - `claude`: Claude Code profile (as `cutex login --cli claude`).
pub fn create_profile(request: &NewProfile) -> Result<StoredAccount, LifecycleError> {
    let name = validate_profile_name(&request.name).map_err(LifecycleError::Invalid)?;
    if request.kind != NewProfileKind::ApiKey
        && (request.preset.is_some() || request.base_url.is_some() || request.api_key.is_some())
    {
        return Err(LifecycleError::Invalid(format!(
            "preset, baseUrl and apiKey are only valid for kind \"apiKey\" (got \"{}\")",
            request.kind.as_str()
        )));
    }
    let _guard = lock();
    let mut store = load_store()?;
    ensure_available(&store, &name, None)?;

    let mut account;
    match request.kind {
        NewProfileKind::Claude => {
            account = new_claude_account(&name);
            if let Some(model) = &request.model {
                account.options.claude.model = model.clone();
            }
        }
        NewProfileKind::Chatgpt => {
            account = base_account(&name, CliKind::Codex, "official");
            let config = request
                .model
                .as_ref()
                .map(|model| -> anyhow::Result<String> {
                    let mut root = toml::value::Table::new();
                    root.insert("model".into(), toml::Value::String(model.clone()));
                    Ok(toml::to_string_pretty(&root)?)
                })
                .transpose()?;
            if let Err(error) = materialize_new_account_files(&account, None, config, None) {
                remove_profile_dir(&account);
                return Err(error.into());
            }
        }
        NewProfileKind::ApiKey => {
            let preset = request.preset.unwrap_or_else(|| find_preset("custom").expect("custom preset"));
            account = base_account(&name, CliKind::Codex, "api-key");
            // The provider name is the profile's legacy plan label; the
            // generic custom preset's ("custom") tells the owner nothing.
            if preset.id != crate::profiles::providers::CUSTOM_PRESET_ID {
                account.plan_type = Some(preset.provider_name.to_string());
            }
            let base_url = request.base_url.as_deref().or(preset.base_url);
            // A preset that names its provider's official quota reader
            // (DeepSeek / Moonshot balance, Zhipu Coding Plan 5 h + week,
            // OpenRouter credits) reads the quota from the start, on the
            // preset's own base URL only. No host or provider-name rule.
            crate::profiles::quota::enable_preset_reader(
                &mut account,
                preset.quota_reader,
                preset.base_url,
                request.base_url.as_deref(),
            );
            let mut root = api_key_profile_config(preset.provider_id, preset.provider_name, base_url)?;
            // The request's model, else the preset's default (presets with a
            // spec already wrote it).
            if let Some(model) = request.model.as_deref().or(preset.default_model) {
                root.insert("model".into(), toml::Value::String(model.to_string()));
            }
            let config = toml::to_string_pretty(&root).map_err(anyhow::Error::from)?;
            // Every preset with a catalog writes it (DeepSeek, GLM, Kimi, MiniMax, xAI).
            let catalog = crate::profiles::preset_spec::catalog_for_provider(preset.provider_id);
            let auth = request
                .api_key
                .as_ref()
                .map(|key| json!({ "OPENAI_API_KEY": key.0, "tokens": null }).to_string());
            if let Err(error) = materialize_new_account_files(&account, auth.as_deref(), Some(config), catalog.as_deref()) {
                remove_profile_dir(&account);
                return Err(error.into());
            }
        }
    }

    store.accounts.push(account.clone());
    if store.active_account_id.is_none() {
        store.active_account_id = Some(account.id.clone());
    }
    if let Err(error) = save_store(&store) {
        remove_profile_dir(&account);
        return Err(error.into());
    }
    Ok(account)
}

/// Copy a profile's settings (store options, config.toml, local model
/// catalog) under a new name, like `cutex profile copy`, but WITHOUT
/// credentials: no auth.json is copied (the copy is logged out / has no API
/// key) and a Claude copy does not get the source's Claude credentials.
/// Account identity (email, ChatGPT plan) is not copied either.
pub fn duplicate_profile(source: &str, name: &str) -> Result<StoredAccount, LifecycleError> {
    let name = validate_profile_name(name).map_err(LifecycleError::Invalid)?;
    let _guard = lock();
    let mut store = load_store()?;
    let index = store
        .accounts
        .iter()
        .position(|account| account.id == source || account.name == source)
        .ok_or(LifecycleError::NotFound)?;
    ensure_available(&store, &name, None)?;
    let source_account = store.accounts[index].clone();
    let mut copy = source_account.clone();
    copy.id = uuid::Uuid::new_v4().to_string();
    copy.name = name;
    copy.last_used_at = None;
    copy.email = None;
    if copy.source.as_deref() != Some("api-key") {
        copy.plan_type = None;
        copy.plan_label = None;
    }

    if copy.cli_kind == CliKind::Codex {
        if let Err(error) = copy_codex_config_files(&source_account, &copy) {
            remove_profile_dir(&copy);
            return Err(error.into());
        }
    }
    store.accounts.insert(index + 1, copy.clone());
    if let Err(error) = save_store(&store) {
        remove_profile_dir(&copy);
        return Err(error.into());
    }
    Ok(copy)
}

fn copy_codex_config_files(source: &StoredAccount, target: &StoredAccount) -> anyhow::Result<()> {
    let source_files = materialized_account_files(source)?;
    let target_files = materialized_account_files(target)?;
    ensure_materialized_account_dir(&target_files)?;
    let mut config = read_optional_text(&source_files.config_path)?;
    if let Some(raw) = config.as_deref() {
        let mut root = parse_toml_table(raw)?;
        if root.contains_key("model_catalog_json") && !profile_uses_local_model_catalog(raw)? {
            // Management edits select immutable catalog files. A duplicate owns
            // its copy (and the source's overlay of an official list), so
            // moving/deleting the source profile cannot break it.
            let catalog = crate::profiles::catalog_api::load(source)?.catalog
                .ok_or_else(|| anyhow::anyhow!("source profile's selected model catalog is missing"))?;
            let dir = profile_dir(target)?;
            let path = crate::profiles::catalog_overlay::write_effective_file(&dir, &catalog)?;
            if let Some(overlay) = crate::profiles::catalog_overlay::load(source)? {
                write_private_pretty_json_atomic(
                    &dir.join(crate::profiles::catalog_overlay::OVERLAY_FILE), &overlay, "duplicated model overlay")?;
            }
            root.insert("model_catalog_json".into(), toml::Value::String(path.to_string_lossy().into_owned()));
            config = Some(toml::to_string_pretty(&root)?);
        }
    }
    write_optional_text_if_changed(&target_files.config_path, config.as_deref())?;
    let uses_local_catalog = config
        .as_deref()
        .map(profile_uses_local_model_catalog)
        .transpose()?
        .unwrap_or(false);
    if uses_local_catalog {
        // A missing local catalog of a preset provider is the preset's own.
        let catalog = match read_optional_text(&source_files.model_catalog_path)? {
            Some(catalog) => Some(catalog),
            None => config
                .as_deref()
                .and_then(|config| parse_toml_table(config).ok())
                .and_then(|table| table.get("model_provider").and_then(toml::Value::as_str).map(str::to_string))
                .and_then(|provider| crate::profiles::preset_spec::catalog_for_provider(&provider)),
        };
        write_optional_text_if_changed(&target_files.model_catalog_path, catalog.as_deref())?;
    }
    refresh_materialized_account_config(target)?;
    Ok(())
}

/// This host's sessions referencing `name`: configured profile and/or the
/// profile a currently bound runtime was launched on. Active and retired
/// sessions. Records of another host are left out: their profile names
/// that host's profiles (they launch only there), and they cannot be
/// deleted or switched here, so they must not block this host's profiles.
pub fn session_references(sessions: &CutexSessionStore, name: &str) -> Vec<SessionRef> {
    let host = crate::platform::host::current_host_name();
    let mut refs: Vec<SessionRef> = sessions
        .sessions
        .values()
        .filter(|record| crate::runtime::lifecycle::cutex_session_host_is_local(&record.host_id, &host))
        .filter_map(|record| {
            let configured = record.profile.as_deref() == Some(name);
            let launched = launched_profile(record).as_deref() == Some(name);
            (configured || launched).then(|| SessionRef {
                cutex_session_id: record.cutex_session_id.clone(),
                display_name: crate::session::metadata::cutex_session_presented_name(record),
                lifecycle: record.archive_state.label().to_string(),
                configured,
                launched,
            })
        })
        .collect();
    refs.sort_by(|a, b| a.cutex_session_id.cmp(&b.cutex_session_id));
    refs
}

/// The current name of the profile the bound runtime was launched on (by
/// id / rename history: bindings keep their launch-time name).
fn launched_profile(record: &CutexSessionRecord) -> Option<String> {
    record
        .app_server_runtime
        .as_ref()
        .and_then(|binding| binding.current_launched_profile())
}

#[derive(Debug)]
pub struct DeleteOutcome {
    pub removed: StoredAccount,
    /// Where the profile directory (including its credentials) was moved.
    pub backup_path: Option<PathBuf>,
    /// The profile was removed but its files could not be moved aside.
    pub backup_error: Option<String>,
    pub references: ProfileReferenceChanges,
    /// Keys of the retired sessions whose stored profile was cleared
    /// (`ArchivedReferences::Detach`).
    pub detached_sessions: Vec<String>,
}

/// What a profile delete does with archived (retired) sessions that still
/// reference the profile.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ArchivedReferences {
    /// They refuse the delete like active ones (`profile_in_use`).
    #[default]
    Refuse,
    /// `?archived=detach`: when only retired sessions reference the profile,
    /// their stored profile is cleared (a restore launches on the default
    /// profile) and the delete proceeds. Active references still refuse.
    Detach,
}

/// Delete a profile. Refuses the default profile and any profile still
/// referenced by an active or retired session. The profile directory
/// (auth.json / Claude credentials included) is moved to
/// `<config dir>/deleted-profiles/<UTC timestamp>-<id>/` with the stored
/// profile record as `profile.json`; quick-run state, the quick-chat
/// default and per-profile new-session defaults that named it are cleared.
pub fn delete_profile(target: &str) -> Result<DeleteOutcome, LifecycleError> {
    delete_profile_with(target, ArchivedReferences::Refuse)
}

/// [`delete_profile`] with a choice for archived references (see
/// [`ArchivedReferences`]); `DeleteOutcome::detached_sessions` lists the
/// retired sessions whose profile was cleared.
pub fn delete_profile_with(target: &str, archived: ArchivedReferences) -> Result<DeleteOutcome, LifecycleError> {
    let _guard = lock();
    let mut store = load_store()?;
    let account = find(&store, target).cloned().ok_or(LifecycleError::NotFound)?;
    let mut config = load_codez_config_checked()?;
    if config.default_profile.as_deref() == Some(account.name.as_str()) {
        return Err(LifecycleError::IsDefault(account.name));
    }
    let detached_sessions = detach_or_refuse(&account.name, archived)?;

    store.accounts.retain(|candidate| candidate.id != account.id);
    if store.active_account_id.as_deref() == Some(account.id.as_str()) {
        store.active_account_id = store.accounts.first().map(|next| next.id.clone());
    }
    let mut state = load_quick_state();
    let references = ProfileReferenceChanges {
        quick_state_changed: remove_profile_references(&mut state, &account.name),
        global_config_changed: remove_global_profile_references(&mut config, &account.name),
        session_keys: detached_sessions.clone(),
    };
    save_store(&store)?;
    if references.global_config_changed {
        save_codez_config(&config)?;
    }
    if references.quick_state_changed {
        save_quick_state(&state)?;
    }
    // profile-avatar: the picture / intro portrait go with the profile.
    crate::profiles::avatar::remove_default(&account.id);
    let (backup_path, backup_error) = match move_to_deleted(&account) {
        Ok(path) => (Some(path), None),
        Err(error) => (None, Some(format!("{error:#}"))),
    };
    Ok(DeleteOutcome { removed: account, backup_path, backup_error, references, detached_sessions })
}

/// The session-store part of a delete: refuses while references remain
/// (all of them, or with `Detach` the active ones), else clears the stored
/// profile of the retired sessions that name it (CAS on the session store,
/// retried) and returns their keys. A retired session whose stale runtime
/// binding was launched on the profile keeps the binding (history only: a
/// restore relaunches from the stored profile).
fn detach_or_refuse(name: &str, archived: ArchivedReferences) -> Result<Vec<String>, LifecycleError> {
    let mut attempt = 0;
    loop {
        let mut sessions = crate::session::store::load_cutex_session_store()?;
        let refs = session_references(&sessions, name);
        if refs.is_empty() {
            return Ok(Vec::new());
        }
        if archived == ArchivedReferences::Refuse || refs.iter().any(|r| r.lifecycle != "retired") {
            return Err(LifecycleError::InUse(refs));
        }
        let timestamp = chrono::Utc::now().to_rfc3339();
        let mut keys = Vec::new();
        for reference in refs.iter().filter(|r| r.configured) {
            let Some((key, record)) = sessions
                .sessions
                .iter_mut()
                .find(|(_, record)| record.cutex_session_id == reference.cutex_session_id)
            else {
                continue;
            };
            record.profile = None;
            record.bump_durable_revision()?;
            record.updated_at = timestamp.clone();
            keys.push(key.clone());
        }
        keys.sort();
        if keys.is_empty() {
            return Ok(keys);
        }
        match crate::session::store::save_cutex_session_store(&sessions) {
            Ok(()) => {
                for key in &keys {
                    if let Err(error) = crate::session::im_bridge::persist_cutex_session_im_record(&sessions, key) {
                        eprintln!("profile delete: IM record of {key} not refreshed: {error:#}");
                    }
                }
                return Ok(keys);
            }
            Err(error)
                if attempt + 1 < SESSION_STORE_RETRIES
                    && error
                        .downcast_ref::<crate::session::store::CutexSessionStoreRevisionConflict>()
                        .is_some() =>
            {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(25 * attempt as u64));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn create_private_dir_all(path: &Path) -> anyhow::Result<()> {
    crate::profiles::materialize::create_private_dir_all(path)?;
    #[cfg(windows)]
    crate::platform::private_fs::secure_directory(path)
        .map_err(|error| anyhow::anyhow!("securing {}: {error:?}", path.display()))?;
    Ok(())
}

fn move_to_deleted(account: &StoredAccount) -> anyhow::Result<PathBuf> {
    let root = crate::config::paths::config_dir()?.join(DELETED_PROFILES_DIR);
    create_private_dir_all(&root)?;
    let backup = root.join(format!(
        "{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
        account.id
    ));
    let source = profile_dir(account)?;
    if source.exists() {
        std::fs::rename(&source, &backup).map_err(|error| {
            anyhow::anyhow!("moving {} to {}: {error}", source.display(), backup.display())
        })?;
    } else {
        create_private_dir_all(&backup)?;
    }
    write_private_pretty_json_atomic(&backup.join("profile.json"), account, "deleted profile record")?;
    Ok(backup)
}

#[derive(Debug)]
pub struct RenameOutcome {
    pub old_name: String,
    pub account: StoredAccount,
    pub changed: bool,
    pub references: ProfileReferenceChanges,
}

/// Rename a profile and every durable reference to it: session records'
/// configured profile (one compare-and-swap write of the session store), the
/// default / quick-chat default profile, per-profile new-session defaults and
/// quick-run state. Works while runtimes launched on the profile run. Their
/// runtime bindings are NOT rewritten: a binding is occurrence evidence that
/// launch receipts (ExternalInput ingress, native stop, receipt retention)
/// and the reviewed registration compare byte for byte, so relabeling it
/// would cut a running agent off its ingress until relaunch. Readers resolve
/// the launched profile by `launched_profile_id`, or (binding launched by
/// r217) through the rename history at the binding's `started_at`
/// (`CutexAppServerRuntimeBinding::current_launched_profile`). No alias for
/// inputs: the old name stops resolving. The rename is appended to the
/// rename history. On a failed later step the earlier writes are reverted.
pub fn rename_profile(target: &str, new_name: &str) -> Result<RenameOutcome, LifecycleError> {
    let new_name = validate_profile_name(new_name).map_err(LifecycleError::Invalid)?;
    let _guard = lock();
    let mut store = load_store()?;
    let index = store
        .accounts
        .iter()
        .position(|account| account.id == target || account.name == target)
        .ok_or(LifecycleError::NotFound)?;
    let old_name = store.accounts[index].name.clone();
    let profile_id = store.accounts[index].id.clone();
    if old_name == new_name {
        return Ok(RenameOutcome {
            old_name,
            account: store.accounts[index].clone(),
            changed: false,
            references: ProfileReferenceChanges::default(),
        });
    }
    ensure_available(&store, &new_name, Some(&profile_id))?;

    // 1. Session records (CAS on the whole session store, retried).
    let (sessions, references, mut state, mut config) =
        rename_in_session_store(&old_name, &new_name)?;

    // 2. Profile store.
    store.accounts[index].name = new_name.clone();
    if let Err(error) = save_store(&store) {
        revert_sessions(&new_name, &old_name, &references);
        return Err(error.into());
    }

    // 3. Global config + quick-run state.
    let global = (|| -> anyhow::Result<()> {
        if references.global_config_changed {
            save_codez_config(&config)?;
        }
        if references.quick_state_changed {
            save_quick_state(&state)?;
        }
        Ok(())
    })();
    if let Err(error) = global {
        store.accounts[index].name = old_name.clone();
        let _ = save_store(&store);
        crate::profiles::references::rename_global_profile_references(&mut config, &new_name, &old_name);
        crate::profiles::references::rename_profile_references(&mut state, &new_name, &old_name);
        let _ = save_codez_config(&config);
        let _ = save_quick_state(&state);
        revert_sessions(&new_name, &old_name, &references);
        return Err(error.into());
    }
    if let Err(error) = crate::profiles::identity::record_rename(&profile_id, &old_name, &new_name) {
        eprintln!("profile rename: rename history not updated: {error:#}");
    }
    for key in &references.session_keys {
        if let Err(error) = crate::session::im_bridge::persist_cutex_session_im_record(&sessions, key) {
            eprintln!("profile rename: IM record for {key} not updated: {error:#}");
        }
    }
    Ok(RenameOutcome {
        old_name,
        account: store.accounts[index].clone(),
        changed: true,
        references,
    })
}

type RenamedState = (
    CutexSessionStore,
    ProfileReferenceChanges,
    crate::profiles::model::QuickRunState,
    crate::profiles::model::CodezConfig,
);

fn rename_in_session_store(old: &str, new: &str) -> Result<RenamedState, LifecycleError> {
    let mut attempt = 0;
    loop {
        let mut sessions = crate::session::store::load_cutex_session_store()?;
        let mut state = load_quick_state();
        let mut config = load_codez_config_checked()?;
        let references = rename_all_profile_references(&mut state, &mut config, &mut sessions, old, new)?;
        if references.session_keys.is_empty() {
            return Ok((sessions, references, state, config));
        }
        match crate::session::store::save_cutex_session_store(&sessions) {
            Ok(()) => return Ok((sessions, references, state, config)),
            Err(error)
                if attempt + 1 < SESSION_STORE_RETRIES
                    && error
                        .downcast_ref::<crate::session::store::CutexSessionStoreRevisionConflict>()
                        .is_some() =>
            {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(25 * attempt as u64));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn revert_sessions(new: &str, old: &str, references: &ProfileReferenceChanges) {
    if references.session_keys.is_empty() {
        return;
    }
    if let Err(error) = rename_in_session_store(new, old) {
        eprintln!("profile rename rollback: session records still name {new}: {error}");
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::MutexGuard;

    /// Private HOME for tests that touch the profile store (serialized).
    pub(crate) struct TestHome {
        root: PathBuf,
        previous: Option<OsString>,
        _guard: MutexGuard<'static, ()>,
    }

    impl TestHome {
        pub(crate) fn new() -> Self {
            let guard = crate::config::paths::test_home_env_lock();
            let root = std::env::temp_dir()
                .join(format!("cutex-profile-lifecycle-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
            std::fs::create_dir_all(root.join(".cutex")).unwrap();
            let previous = std::env::var_os("HOME");
            unsafe { std::env::set_var("HOME", &root) };
            Self { root, previous, _guard: guard }
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            unsafe {
                match self.previous.take() {
                    Some(value) => std::env::set_var("HOME", value),
                    None => std::env::remove_var("HOME"),
                }
            }
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn api_key(name: &str, preset: &str, key: Option<&str>) -> NewProfile {
        NewProfile {
            name: name.into(),
            kind: NewProfileKind::ApiKey,
            preset: find_preset(preset),
            base_url: None,
            model: None,
            api_key: key.map(|key| ApiKey(key.into())),
        }
    }

    fn session(key: &str, profile: &str) -> CutexSessionRecord {
        CutexSessionRecord::new_at(
            key.to_string(),
            None,
            crate::platform::host::current_host_name(),
            "/tmp".to_string(),
            Some(profile.to_string()),
            "2026-10-07T00:00:00Z".to_string(),
        )
        .expect("session fixture")
    }

    #[test]
    fn names_are_validated() {
        assert_eq!(validate_profile_name("  GLM 2 ").unwrap(), "GLM 2");
        for bad in ["", "   ", "a/b", "a\\b", "tab\there", &"x".repeat(65)] {
            assert!(validate_profile_name(bad).is_err(), "{bad:?}");
        }
        assert_eq!(NewProfileKind::parse("apiKey"), Some(NewProfileKind::ApiKey));
        assert_eq!(NewProfileKind::parse("api-key"), None);
    }

    #[test]
    fn create_writes_the_cli_layout_and_rejects_duplicates() {
        let _home = TestHome::new();
        let glm = create_profile(&api_key("GLM", "glm", Some("sk-test-only"))).expect("create apiKey");
        assert_eq!(glm.source.as_deref(), Some("api-key"));
        let files = materialized_account_files(&glm).unwrap();
        let config = std::fs::read_to_string(&files.config_path).unwrap();
        assert!(config.contains("model_provider = \"zai\""), "{config}");
        assert!(config.contains("https://api.z.ai/api/v1"), "{config}");
        assert!(config.contains("model = \"glm-5.3\""), "{config}");
        assert!(files.auth_path.exists());
        // GLM's catalog is written at creation and parses as a native catalog.
        let catalog: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&files.model_catalog_path).unwrap()).unwrap();
        crate::profiles::known_models::tests::assert_native_model_rows(&catalog);
        assert_eq!(catalog["models"][0]["slug"], "glm-5.3");
        // Zhipu Coding Plan quota: 5 h primary + weekly extra.
        assert!(glm.options.quota.enabled);
        assert_eq!(glm.options.quotas.len(), 1);
        assert_eq!(glm.options.quotas[0].window_label.as_deref(), Some("week"));
        assert_eq!(crate::profiles::providers::provider_family(&glm).0, "zhipu");

        let custom = create_profile(&NewProfile {
            base_url: Some("https://llm.example.com/v1".into()),
            model: Some("m-1".into()),
            ..api_key("mine", "custom", None)
        })
        .expect("create custom without key");
        let files = materialized_account_files(&custom).unwrap();
        assert!(!files.auth_path.exists(), "no key, no auth.json");
        let config = std::fs::read_to_string(&files.config_path).unwrap();
        assert!(config.contains("model = \"m-1\"") && config.contains("llm.example.com"), "{config}");
        assert_eq!(crate::profiles::providers::provider_family(&custom).0, "custom:llm.example.com");
        // The generic preset writes no plan label; named presets do.
        assert_eq!(custom.plan_type, None);

        let deepseek = create_profile(&api_key("ds", "deepseek", None)).expect("deepseek");
        assert_eq!(deepseek.plan_type.as_deref(), Some(crate::profiles::deepseek::DEEPSEEK_PROVIDER_NAME));
        assert!(materialized_account_files(&deepseek).unwrap().model_catalog_path.exists());

        let chatgpt = create_profile(&NewProfile {
            name: "work".into(),
            kind: NewProfileKind::Chatgpt,
            preset: None,
            base_url: None,
            model: None,
            api_key: None,
        })
        .expect("chatgpt");
        assert_eq!(chatgpt.source.as_deref(), Some("official"));
        assert!(!materialized_account_files(&chatgpt).unwrap().auth_path.exists());
        assert_eq!(crate::profiles::providers::provider_family(&chatgpt).0, "openai-chatgpt");

        let claude = create_profile(&NewProfile {
            name: "claude".into(),
            kind: NewProfileKind::Claude,
            preset: None,
            base_url: None,
            model: None,
            api_key: None,
        })
        .expect("claude");
        assert_eq!(claude.cli_kind, CliKind::Claude);

        assert!(matches!(create_profile(&api_key("GLM", "zai", None)), Err(LifecycleError::NameConflict(_))));
        assert!(matches!(
            create_profile(&api_key(&glm.id, "zai", None)),
            Err(LifecycleError::NameConflict(_))
        ));
        assert!(matches!(
            create_profile(&NewProfile { kind: NewProfileKind::Chatgpt, ..api_key("x", "zai", None) }),
            Err(LifecycleError::Invalid(_))
        ));
        let store = load_store().unwrap();
        assert_eq!(store.accounts.len(), 5);
        assert_eq!(store.active_account_id.as_deref(), Some(glm.id.as_str()));
    }

    #[test]
    fn presets_with_a_quota_reader_start_with_it_enabled() {
        use crate::profiles::quota::{config_view, Preset, Reader};
        let _home = TestHome::new();
        // DeepSeek preset: its balance reader on, resolved endpoint stored.
        let deepseek = create_profile(&api_key("ds", "deepseek", Some("sk-test-only"))).unwrap();
        assert_eq!((deepseek.quota_preset, deepseek.quota_reader), (Preset::Custom, Some(Reader::DeepseekBalance)));
        assert!(deepseek.options.quota.enabled);
        assert_eq!(deepseek.options.quota.url, "https://api.deepseek.com/user/balance", "older binaries read it");
        let view = config_view(&deepseek);
        assert_eq!(view["preset"], "deepseek_balance");
        assert_eq!(view["resolvedPrimary"]["url"], "https://api.deepseek.com/user/balance");
        assert_eq!(view["presetError"], serde_json::Value::Null);
        let stored = load_store().unwrap();
        let stored = stored.accounts.iter().find(|a| a.id == deepseek.id).unwrap();
        assert!(stored.options.quota.enabled && stored.quota_reader == Some(Reader::DeepseekBalance), "persisted");
        // A custom preset on any relay: no reader, whatever the host.
        let relay = create_profile(&NewProfile {
            base_url: Some("https://relay.example/v1".into()),
            ..api_key("relay", "custom", None)
        })
        .unwrap();
        assert_eq!((relay.quota_preset, relay.quota_reader), (Preset::Custom, None));
        assert!(!relay.options.quota.enabled);
        // Official Moonshot (Kimi): the preset of each platform names its reader.
        let kimi = create_profile(&api_key("k", "moonshot", None)).unwrap();
        assert_eq!(kimi.quota_reader, Some(Reader::MoonshotBalance));
        let view = config_view(&kimi);
        assert_eq!(view["resolvedPrimary"]["url"], "https://api.moonshot.ai/v1/users/me/balance");
        assert_eq!(view["resolvedPrimary"]["label"], "Kimi");
        let kimi_cn = create_profile(&api_key("k cn", "moonshot-cn", None)).unwrap();
        assert_eq!(config_view(&kimi_cn)["resolvedPrimary"]["url"], "https://api.moonshot.cn/v1/users/me/balance");
        // The global preset on another base URL: no host rule, quota off.
        let moved = create_profile(&NewProfile {
            base_url: Some("https://api.moonshot.cn/v1".into()),
            ..api_key("k moved", "kimi", None)
        })
        .unwrap();
        assert_eq!(moved.quota_reader, None);
        assert!(!moved.options.quota.enabled);
        // Official GLM (Z.ai / Zhipu): the Coding Plan quota API; OpenRouter:
        // key credits.
        for (name, preset, url) in [
            ("z", "zai", "https://api.z.ai/api/monitor/usage/quota/limit"),
            ("zp", "zhipu", "https://open.bigmodel.cn/api/monitor/usage/quota/limit"),
            ("or", "openrouter", "https://openrouter.ai/api/v1/key"),
        ] {
            let profile = create_profile(&api_key(name, preset, None)).unwrap();
            assert!(profile.quota_reader.is_some(), "{preset}");
            assert_eq!(config_view(&profile)["resolvedPrimary"]["url"], url, "{preset}");
        }
        // Kimi Code membership plan (both regions): the /usages reader on the
        // profile's own base URL, plus the optional week and month windows.
        for (name, preset, url) in [
            ("kc", "kimi-code", "https://api.kimi.ai/coding/v1/usages"),
            ("kc cn", "kimi-code-cn", "https://api.kimi.com/coding/v1/usages"),
        ] {
            let profile = create_profile(&api_key(name, preset, None)).unwrap();
            assert_eq!(profile.quota_reader, Some(Reader::KimiCodePlan), "{preset}");
            let view = config_view(&profile);
            assert_eq!(view["resolvedPrimary"]["url"], url, "{preset}");
            assert_eq!(view["resolvedPrimary"]["window_label"], "5h", "{preset}");
            let windows: Vec<_> = profile.options.quotas.iter().map(|c| (c.window_label.clone().unwrap(), c.optional)).collect();
            assert_eq!(windows, [("week".to_string(), true), ("month".to_string(), true)], "{preset}");
        }
        // Other APIs: no reader, left off.
        for (name, preset) in [("oa", "openai"), ("mm", "minimax"), ("x", "xai")] {
            let profile = create_profile(&api_key(name, preset, None)).unwrap();
            assert_eq!((profile.quota_preset, profile.quota_reader), (Preset::Custom, None), "{preset}");
            assert!(!profile.options.quota.enabled, "{preset}");
        }
        let chatgpt = create_profile(&NewProfile {
            name: "work".into(),
            kind: NewProfileKind::Chatgpt,
            preset: None,
            base_url: None,
            model: None,
            api_key: None,
        })
        .unwrap();
        assert!(!chatgpt.options.quota.enabled);
    }

    #[test]
    fn duplicate_copies_config_but_not_credentials() {
        let _home = TestHome::new();
        let source = create_profile(&api_key("ds", "deepseek", Some("sk-test-only"))).unwrap();
        let copy = duplicate_profile("ds", "ds copy").expect("duplicate");
        assert_ne!(copy.id, source.id);
        let files = materialized_account_files(&copy).unwrap();
        assert!(!files.auth_path.exists(), "credentials must not be copied");
        assert!(files.model_catalog_path.exists());
        let config = std::fs::read_to_string(&files.config_path).unwrap();
        assert!(config.contains("model_provider = \"deepseek\""), "{config}");
        let store = load_store().unwrap();
        assert_eq!(store.accounts[1].name, "ds copy", "inserted after the source");
        assert!(matches!(duplicate_profile("ds", "ds copy"), Err(LifecycleError::NameConflict(_))));
        assert!(matches!(duplicate_profile("nope", "x"), Err(LifecycleError::NotFound)));
    }

    #[test]
    fn delete_refuses_default_and_referenced_then_moves_files_aside() {
        let _home = TestHome::new();
        let keep = create_profile(&api_key("keep", "zai", Some("sk-test-only"))).unwrap();
        let gone = create_profile(&api_key("gone", "moonshot", Some("sk-test-only"))).unwrap();
        let mut config = load_codez_config_checked().unwrap();
        config.default_profile = Some("keep".into());
        config.quick_chat_default_profile = Some("gone".into());
        save_codez_config(&config).unwrap();
        assert!(matches!(delete_profile("keep"), Err(LifecycleError::IsDefault(_))));

        let mut sessions = CutexSessionStore::default();
        sessions.sessions.insert("cutex.s1".into(), session("cutex.s1", "gone"));
        crate::session::store::save_cutex_session_store(&sessions).unwrap();
        match delete_profile(&gone.id) {
            Err(LifecycleError::InUse(refs)) => {
                assert_eq!(refs.len(), 1);
                assert_eq!(refs[0].cutex_session_id, "cutex.s1");
                assert!(refs[0].configured && !refs[0].launched);
                assert_eq!(refs[0].lifecycle, "active");
            }
            other => panic!("expected InUse, got {other:?}"),
        }
        let mut sessions = crate::session::store::load_cutex_session_store().unwrap();
        sessions.sessions.clear();
        crate::session::store::save_cutex_session_store(&sessions).unwrap();

        let outcome = delete_profile("gone").expect("delete");
        let backup = outcome.backup_path.expect("moved aside");
        assert!(backup.join("auth.json").exists() && backup.join("profile.json").exists());
        assert!(!profile_dir(&gone).unwrap().exists());
        assert!(outcome.references.global_config_changed);
        assert_eq!(load_codez_config_checked().unwrap().quick_chat_default_profile, None);
        let store = load_store().unwrap();
        assert_eq!(store.accounts.len(), 1);
        assert_eq!(store.accounts[0].id, keep.id);
        assert!(matches!(delete_profile("gone"), Err(LifecycleError::NotFound)));
    }

    #[test]
    fn delete_detaches_archived_references_only_on_request_and_ignores_other_hosts() {
        let _home = TestHome::new();
        create_profile(&api_key("keep", "zai", Some("sk-test-only"))).unwrap();
        create_profile(&api_key("birch", "zai", Some("sk-test-only"))).unwrap();
        let mut config = load_codez_config_checked().unwrap();
        config.default_profile = Some("keep".into());
        save_codez_config(&config).unwrap();

        let mut sessions = CutexSessionStore::default();
        let mut archived = session("cutex.archived", "birch");
        archived.archive_state = crate::session::model::CutexSessionArchiveState::Retired;
        sessions.sessions.insert("cutex.archived".into(), archived);
        let mut elsewhere = session("cutex.elsewhere", "birch");
        elsewhere.host_id = format!("{}-other-host", crate::platform::host::current_host_name());
        sessions.sessions.insert("cutex.elsewhere".into(), elsewhere);
        sessions.sessions.insert("cutex.active".into(), session("cutex.active", "birch"));
        crate::session::store::save_cutex_session_store(&sessions).unwrap();

        // Another host's record never counts; an active one refuses even with Detach.
        match delete_profile_with("birch", ArchivedReferences::Detach) {
            Err(LifecycleError::InUse(refs)) => {
                let ids: Vec<_> = refs.iter().map(|r| r.cutex_session_id.as_str()).collect();
                assert_eq!(ids, ["cutex.active", "cutex.archived"]);
            }
            other => panic!("expected InUse, got {other:?}"),
        }
        let mut sessions = crate::session::store::load_cutex_session_store().unwrap();
        sessions.sessions.remove("cutex.active");
        crate::session::store::save_cutex_session_store(&sessions).unwrap();

        // Only archived left: the default still refuses, Detach clears them and deletes.
        assert!(matches!(delete_profile("birch"), Err(LifecycleError::InUse(refs)) if refs.len() == 1));
        let revision = sessions.sessions["cutex.archived"].durable_revision();
        let outcome = delete_profile_with("birch", ArchivedReferences::Detach).expect("delete");
        assert_eq!(outcome.detached_sessions, ["cutex.archived"]);
        let sessions = crate::session::store::load_cutex_session_store().unwrap();
        let archived = &sessions.sessions["cutex.archived"];
        assert_eq!(archived.profile, None);
        assert!(archived.durable_revision() > revision);
        assert_eq!(sessions.sessions["cutex.elsewhere"].profile.as_deref(), Some("birch"), "other host untouched");
        assert!(find(&load_store().unwrap(), "birch").is_none());
    }

    #[test]
    fn rename_updates_references_and_keeps_running_runtime_bindings() {
        let _home = TestHome::new();
        create_profile(&api_key("old", "zai", Some("sk-test-only"))).unwrap();
        create_profile(&api_key("other", "zai", Some("sk-test-only"))).unwrap();
        let mut config = load_codez_config_checked().unwrap();
        config.default_profile = Some("old".into());
        save_codez_config(&config).unwrap();
        let mut sessions = CutexSessionStore::default();
        sessions.sessions.insert("cutex.a".into(), session("cutex.a", "old"));
        sessions.sessions.insert("cutex.b".into(), session("cutex.b", "other"));
        crate::session::store::save_cutex_session_store(&sessions).unwrap();

        assert!(matches!(rename_profile("old", "other"), Err(LifecycleError::NameConflict(_))));
        assert!(matches!(rename_profile("old", "a/b"), Err(LifecycleError::Invalid(_))));
        let outcome = rename_profile("old", "new").expect("rename");
        assert!(outcome.changed);
        assert_eq!(outcome.references.session_keys, ["cutex.a"]);
        assert_eq!(load_codez_config_checked().unwrap().default_profile.as_deref(), Some("new"));
        let sessions = crate::session::store::load_cutex_session_store().unwrap();
        assert_eq!(sessions.sessions["cutex.a"].profile.as_deref(), Some("new"));
        assert_eq!(sessions.sessions["cutex.b"].profile.as_deref(), Some("other"));
        assert!(find(&load_store().unwrap(), "new").is_some());
        assert!(!rename_profile("new", "new").unwrap().changed);

        // Live rename: runtimes launched on the profile keep running and their
        // bindings stay byte for byte (receipts / ingress compare them); they
        // resolve the new name by id, or (r217 binding) via the rename history.
        let other_id = find(&load_store().unwrap(), "other").unwrap().id.clone();
        let mut sessions = crate::session::store::load_cutex_session_store().unwrap();
        let binding = |profile: &str, id: Option<&str>| -> crate::session::model::CutexAppServerRuntimeBinding {
            let mut value = json!({
                "transport": "loopback_web_socket", "endpoint": "ws://127.0.0.1:1", "pid": 1,
                "runtime_dir": "/tmp", "launched_profile": profile, "diagnostic_journal_path": "/tmp/j",
                "schema_version": "x", "schema_sha256": "y", "started_at": "2026-10-07T00:00:00Z",
            });
            if let Some(id) = id {
                value["launched_profile_id"] = json!(id);
            }
            serde_json::from_value(value).expect("binding fixture")
        };
        // r217-written binding: name only; r218 binding: name + id.
        sessions.sessions.get_mut("cutex.b").unwrap().app_server_runtime = Some(binding("other", None));
        let mut r218 = session("cutex.c", "other");
        r218.app_server_runtime = Some(binding("other", Some(&other_id)));
        sessions.sessions.insert("cutex.c".into(), r218);
        crate::session::store::save_cutex_session_store(&sessions).unwrap();

        let outcome = rename_profile("other", "renamed").expect("live rename");
        assert_eq!(outcome.references.session_keys, ["cutex.b", "cutex.c"]);
        let sessions = crate::session::store::load_cutex_session_store().unwrap();
        let r217_binding = sessions.sessions["cutex.b"].app_server_runtime.clone().unwrap();
        let r218_binding = sessions.sessions["cutex.c"].app_server_runtime.clone().unwrap();
        assert_eq!(r217_binding, binding("other", None), "binding untouched");
        assert_eq!(r218_binding, binding("other", Some(&other_id)), "binding untouched");
        assert_eq!(r217_binding.current_launched_profile().as_deref(), Some("renamed"));
        assert_eq!(r217_binding.current_launched_profile_ref().id.as_deref(), Some(other_id.as_str()));
        assert_eq!(r218_binding.current_launched_profile().as_deref(), Some("renamed"));
        // Both still count as launched on the renamed profile (delete refuses).
        let refs = session_references(&sessions, "renamed");
        assert!(refs.iter().filter(|r| r.launched).count() == 2, "{refs:?}");
        assert!(matches!(delete_profile("renamed"), Err(LifecycleError::InUse(_))));

        // A live runtime (event context with the launch-time name and the
        // id) now reports under the new name.
        let live = crate::profiles::identity::LaunchedProfile::resolve(Some(&other_id), Some("other"));
        assert_eq!(live.name.as_deref(), Some("renamed"));
        // A pre-r218 context (name only) resolved its id on connect, before
        // the rename; after it the old name no longer resolves (no alias).
        assert_eq!(crate::profiles::identity::id_for_name("other"), None);
        assert!(find(&load_store().unwrap(), "other").is_none());
        // Rename history (usage attribution only).
        let history = crate::profiles::identity::load_history_at(&crate::profiles::identity::history_path().unwrap());
        let last = history.renames.last().unwrap();
        assert_eq!((last.profile_id.as_str(), last.from.as_str(), last.to.as_str()), (other_id.as_str(), "other", "renamed"));

        // A new profile that takes the old name later does not capture the
        // r217 occurrence launched before the rename.
        create_profile(&api_key("other", "zai", Some("sk-test-only"))).unwrap();
        assert_eq!(r217_binding.current_launched_profile().as_deref(), Some("renamed"));
        assert_eq!(r218_binding.current_launched_profile().as_deref(), Some("renamed"));
    }
}
