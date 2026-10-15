//! Local users and grants (P1 user isolation; cutex-desktop
//! docs/research/user-isolation-p1-20261014.md).
//!
//! - Store: `<cutex home>/runtime/users/users-v1.json` (owner-private dir,
//!   atomic writes under `users.lock`), schema `cutex/users/v1`, a global
//!   `revision` bumped on every change.
//! - The built-in user `owner` is the host's owner: full rights, no grant,
//!   cannot be deleted. Every other user has a grant (default: nothing).
//! - Every Management client / device token belongs to a user through
//!   `bindings` (kept here, not in `tokens.json`, so an older cutex that
//!   rewrites `tokens.json` cannot drop a binding). A token without a binding
//!   is *pending*: it authenticates but sees nothing until the owner assigns it.
//! - Migration (once, when the file does not exist): create `owner` and bind
//!   every token that exists at that moment to it; `tokens.json` is copied to
//!   `backup-<time>/` first. Nothing the owner has today changes.
//! - Tokens and token hashes never appear here, in the audit log or in errors.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context;
use fs2::FileExt;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{json, Value};

use crate::management::client_tokens::ClientTokenStore;

pub const OWNER: &str = "owner";
pub const SCHEMA: &str = "cutex/users/v1";
pub const USER_ID_PREFIX: &str = "u-";
const MAX_USERS: usize = 50;
const MAX_GRANT_ITEMS: usize = 500;
const MAX_ID_CHARS: usize = 200;
const MAX_NAME_CHARS: usize = 60;
const CACHE_MAX_AGE: Duration = Duration::from_secs(2);
const SCOPE_MAX_AGE: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------- model

/// `projects`: `"all"` (every local project, never a peer's agents) or a list
/// of local project ids.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Projects {
    All,
    Only(Vec<String>),
}

impl Default for Projects {
    fn default() -> Self {
        Self::Only(Vec::new())
    }
}

impl Serialize for Projects {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::All => serializer.serialize_str("all"),
            Self::Only(ids) => ids.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Projects {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Value::deserialize(deserializer)? {
            Value::String(text) if text == "all" => Ok(Self::All),
            Value::Array(items) => items
                .into_iter()
                .map(|item| match item {
                    Value::String(id) => Ok(id),
                    _ => Err(serde::de::Error::custom("project ids must be strings")),
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Self::Only),
            _ => Err(serde::de::Error::custom(
                "projects must be \"all\" or a list of project ids",
            )),
        }
    }
}

/// What a non-owner user may see and do. Default: nothing, read-only.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Grant {
    #[serde(default)]
    pub projects: Projects,
    #[serde(default)]
    pub standalone_agents: Vec<String>,
    #[serde(default)]
    pub send: bool,
}

impl Grant {
    pub fn validate(&self) -> Result<(), UserError> {
        if let Projects::Only(ids) = &self.projects {
            validate_ids(ids, "projects")?;
        }
        validate_ids(&self.standalone_agents, "standaloneAgents")
    }

    fn normalized(mut self) -> Self {
        if let Projects::Only(ids) = &mut self.projects {
            ids.sort();
            ids.dedup();
        }
        self.standalone_agents.sort();
        self.standalone_agents.dedup();
        self
    }
}

fn validate_ids(ids: &[String], field: &str) -> Result<(), UserError> {
    if ids.len() > MAX_GRANT_ITEMS {
        return Err(UserError::invalid(format!("{field}: at most {MAX_GRANT_ITEMS} ids")));
    }
    if ids.iter().any(|id| {
        id.is_empty()
            || id.chars().count() > MAX_ID_CHARS
            || id.chars().any(|c| c.is_control() || c.is_whitespace())
    }) {
        return Err(UserError::invalid(format!("{field}: invalid id")));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// `None` only for `owner`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<Grant>,
    pub created_at: String,
    pub updated_at: String,
}

impl User {
    pub fn is_owner(&self) -> bool {
        self.id == OWNER
    }

    pub fn view(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "color": self.color,
            "builtin": self.is_owner(),
            "isOwner": self.is_owner(),
            "grant": if self.is_owner() { Value::Null } else { json!(self.grant.clone().unwrap_or_default()) },
            "createdAt": self.created_at,
            "updatedAt": self.updated_at,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default = "yes")]
    pub owner_requires_approval: bool,
}

fn yes() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self { owner_requires_approval: true }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Migration {
    pub migrated_at: String,
    /// Token ids bound to `owner` by the migration (never secrets).
    pub owner_tokens: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub schema: String,
    pub revision: u64,
    pub users: Vec<User>,
    /// tokenId → userId. Absent = pending.
    #[serde(default)]
    pub bindings: BTreeMap<String, String>,
    #[serde(default)]
    pub settings: Settings,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration: Option<Migration>,
}

impl State {
    fn fresh(now: &str) -> Self {
        Self {
            schema: SCHEMA.into(),
            revision: 1,
            users: vec![User {
                id: OWNER.into(),
                name: "Owner".into(),
                color: None,
                grant: None,
                created_at: now.into(),
                updated_at: now.into(),
            }],
            bindings: BTreeMap::new(),
            settings: Settings::default(),
            migration: None,
        }
    }

    pub fn user(&self, id: &str) -> Option<&User> {
        self.users.iter().find(|user| user.id == id)
    }

    fn check(&self) -> anyhow::Result<()> {
        if self.schema != SCHEMA {
            anyhow::bail!("unsupported users store schema");
        }
        let owners = self.users.iter().filter(|user| user.is_owner()).count();
        if owners != 1 || self.users.iter().any(|user| user.is_owner() && user.grant.is_some()) {
            anyhow::bail!("the users store must hold exactly one owner without a grant");
        }
        let mut ids = HashSet::new();
        for user in &self.users {
            if !ids.insert(user.id.as_str()) {
                anyhow::bail!("duplicate user id in the users store");
            }
            if !user.is_owner() && (user.grant.is_none() || !valid_user_id(&user.id)) {
                anyhow::bail!("invalid user record in the users store");
            }
        }
        Ok(())
    }

    /// The user a token belongs to; `None` = pending (unbound, or bound to a
    /// user that no longer exists).
    pub fn user_of_token(&self, token_id: &str) -> Option<&User> {
        self.bindings.get(token_id).and_then(|user| self.user(user))
    }
}

pub fn valid_user_id(id: &str) -> bool {
    id == OWNER
        || (id.starts_with(USER_ID_PREFIX)
            && id.len() == USER_ID_PREFIX.len() + 12
            && id[USER_ID_PREFIX.len()..].bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
    pub details: Value,
}

impl UserError {
    pub fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self { status, code, message: message.into(), details: json!({}) }
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(400, "invalid_request", message)
    }
    fn unavailable(error: anyhow::Error) -> Self {
        Self::new(503, "users_unavailable", format!("the users store is unavailable: {error:#}"))
    }
}

// ---------------------------------------------------------------- principal

/// Who a Management request acts as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Principal {
    /// Root credential, or a token bound to `owner`.
    Owner,
    /// A token bound to a non-owner user.
    User { id: String, name: String, grant: Grant },
    /// A token without a (valid) binding.
    Pending,
}

impl Principal {
    pub fn is_owner(&self) -> bool {
        matches!(self, Self::Owner)
    }

    /// The `user` object of `GET /v2/capabilities`.
    pub fn capability_view(&self, owner_name: Option<&str>) -> Value {
        match self {
            Self::Owner => json!({
                "id": OWNER, "name": owner_name.unwrap_or("Owner"), "isOwner": true,
                "pending": false, "grant": Value::Null,
            }),
            Self::User { id, name, grant } => json!({
                "id": id, "name": name, "isOwner": false, "pending": false, "grant": grant,
            }),
            Self::Pending => json!({
                "id": Value::Null, "name": Value::Null, "isOwner": false, "pending": true,
                "grant": Value::Null,
            }),
        }
    }

    /// Changes whenever what this principal may see changes (streams close).
    pub fn access_stamp(&self) -> String {
        match self {
            Self::Owner => OWNER.into(),
            Self::Pending => "-".into(),
            Self::User { id, grant, .. } => {
                format!("{id}|{}", serde_json::to_string(grant).unwrap_or_default())
            }
        }
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Principal>> = const { RefCell::new(None) };
}

/// The principal of the request the current thread serves. `None` outside a
/// client-token request (root credential, internal callers): the owner.
pub fn current_principal() -> Option<Principal> {
    CURRENT.with(|cell| cell.borrow().clone())
}

pub fn set_current_principal(principal: Option<Principal>) {
    CURRENT.with(|cell| *cell.borrow_mut() = principal);
}

/// The acting user of the current request when it is not the owner
/// (`actingUser` in management events; never shown to agents).
pub fn current_acting_user() -> Option<Value> {
    match current_principal()? {
        Principal::User { id, name, .. } => {
            let mut user = json!({ "id": id, "name": name });
            crate::management::user_profiles::decorate_default(&mut user, &id);
            Some(user)
        }
        _ => None,
    }
}

// ---------------------------------------------------------------- store

#[derive(Clone, Debug)]
pub struct UserStore {
    dir: PathBuf,
    tokens: ClientTokenStore,
}

impl UserStore {
    pub fn open_default() -> anyhow::Result<Self> {
        let home = crate::config::paths::config_dir()?;
        Ok(Self {
            dir: home.join("runtime").join("users"),
            tokens: ClientTokenStore::at(home.join("management-clients")),
        })
    }

    /// The users store that belongs to a client token store
    /// (`<home>/management-clients` → `<home>/runtime/users`).
    pub fn beside(tokens: &ClientTokenStore) -> Self {
        let home = tokens.dir().parent().map(Path::to_path_buf).unwrap_or_default();
        Self { dir: home.join("runtime").join("users"), tokens: tokens.clone() }
    }

    pub fn at(dir: impl Into<PathBuf>, tokens: ClientTokenStore) -> Self {
        Self { dir: dir.into(), tokens }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join("users-v1.json")
    }

    pub fn tokens(&self) -> &ClientTokenStore {
        &self.tokens
    }

    fn read(&self) -> anyhow::Result<Option<State>> {
        match std::fs::read(self.path()) {
            Ok(bytes) => {
                let state: State =
                    serde_json::from_slice(&bytes).context("parse the users store")?;
                state.check()?;
                Ok(Some(state))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("read the users store"),
        }
    }

    /// Run `mutate` under the store lock (migrating first when the store does
    /// not exist yet); writes and bumps `revision` only when something changed.
    pub fn with<T>(
        &self,
        mutate: impl FnOnce(&mut State) -> Result<T, UserError>,
    ) -> Result<T, UserError> {
        crate::management::client_tokens::ensure_private_dir(&self.dir)
            .map_err(UserError::unavailable)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.dir.join("users.lock"))
            .map_err(|e| UserError::unavailable(e.into()))?;
        lock.lock_exclusive().map_err(|e| UserError::unavailable(e.into()))?;
        let mut state = match self.read().map_err(UserError::unavailable)? {
            Some(state) => state,
            None => self.migrate().map_err(UserError::unavailable)?,
        };
        let before = state.clone();
        let result = mutate(&mut state)?;
        // Drop bindings of tokens that no longer exist (revoked elsewhere).
        if let Ok(known) = self.tokens.list() {
            let known: HashSet<String> = known.into_iter().map(|t| t.token_id).collect();
            state.bindings.retain(|token, _| known.contains(token));
        }
        if state != before {
            state.revision = before.revision + 1;
            state.check().map_err(|e| UserError::new(500, "users_invalid", format!("{e:#}")))?;
            crate::config::atomic::write_private_pretty_json_atomic(&self.path(), &state, "users")
                .map_err(UserError::unavailable)?;
        }
        Ok(result)
    }

    /// The one-time migration; the caller holds the lock.
    fn migrate(&self) -> anyhow::Result<State> {
        let now = crate::management::instance::now_utc();
        let tokens = self.tokens.list().context("list client tokens for the users migration")?;
        let mut state = State::fresh(&now);
        let mut backup = None;
        if self.tokens.tokens_path().exists() {
            let stamp = now.replace([':', '-'], "");
            let dir = self.dir.join(format!("backup-{stamp}"));
            crate::management::client_tokens::ensure_private_dir(&dir)?;
            let bytes = std::fs::read(self.tokens.tokens_path())?;
            crate::config::atomic::write_private_bytes_atomic(&dir.join("tokens.json"), &bytes)?;
            backup = Some(dir.file_name().unwrap_or_default().to_string_lossy().into_owned());
        }
        let owner_tokens: Vec<String> = tokens.into_iter().map(|t| t.token_id).collect();
        for token in &owner_tokens {
            state.bindings.insert(token.clone(), OWNER.into());
        }
        state.migration = Some(Migration { migrated_at: now, owner_tokens, backup });
        crate::config::atomic::write_private_pretty_json_atomic(&self.path(), &state, "users")?;
        audit(&self.dir, "migrated", json!({ "ownerTokens": state.bindings.len() }));
        Ok(state)
    }

    /// Current state (migrating when needed), cached by file stamp for at
    /// most 2 s. Errors fail closed at the caller.
    pub fn snapshot(&self) -> anyhow::Result<Arc<State>> {
        struct Cached {
            path: PathBuf,
            stamp: (SystemTime, u64),
            loaded_at: Instant,
            state: Arc<State>,
        }
        static CACHE: OnceLock<Mutex<Option<Cached>>> = OnceLock::new();
        let path = self.path();
        let stamp = |path: &Path| {
            std::fs::metadata(path)
                .ok()
                .map(|meta| (meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), meta.len()))
        };
        if stamp(&path).is_none() {
            self.with(|_| Ok(())).map_err(|e| anyhow::anyhow!(e.message))?;
        }
        let current = stamp(&path).context("the users store is missing after migration")?;
        let mut cache = CACHE
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = cache.as_ref() {
            if cached.path == path
                && cached.stamp == current
                && cached.loaded_at.elapsed() < CACHE_MAX_AGE
            {
                return Ok(cached.state.clone());
            }
        }
        let state = Arc::new(self.read()?.context("the users store is missing")?);
        *cache = Some(Cached { path, stamp: current, loaded_at: Instant::now(), state: state.clone() });
        Ok(state)
    }

    /// The principal of a verified client / device token. Store errors fail
    /// closed (pending).
    pub fn principal_for_token(&self, token_id: &str) -> Principal {
        match self.snapshot() {
            Ok(state) => principal_in(&state, token_id),
            Err(error) => {
                eprintln!("users store unavailable; token treated as pending: {error:#}");
                Principal::Pending
            }
        }
    }

    /// Default binding of a token issued by `cutex management client-token
    /// issue` without `--user`: owner for the host's first paired device (no
    /// client / device token exists at all) or when the owner turned approval
    /// off; otherwise pending.
    pub fn default_cli_binding(&self) -> Result<Option<String>, UserError> {
        let tokens = self.tokens.list().map_err(UserError::unavailable)?;
        self.with(|state| {
            Ok((!state.settings.owner_requires_approval || tokens.is_empty()).then(|| OWNER.to_string()))
        })
    }

    /// Bind (`Some(user)`) or unbind (`None` = pending) a token.
    pub fn bind_token(&self, token_id: &str, user: Option<&str>, actor: &str) -> Result<(), UserError> {
        let exists = self
            .tokens
            .list()
            .map_err(UserError::unavailable)?
            .iter()
            .any(|token| token.token_id == token_id);
        if !exists {
            return Err(UserError::new(404, "client_token_not_found", "no client token with this id"));
        }
        self.with(|state| {
            match user {
                Some(user) => {
                    if state.user(user).is_none() {
                        return Err(UserError::new(404, "user_not_found", "no user with this id"));
                    }
                    state.bindings.insert(token_id.to_string(), user.to_string());
                }
                None => {
                    state.bindings.remove(token_id);
                }
            }
            Ok(())
        })?;
        audit(&self.dir, "tokenBound", json!({ "tokenId": token_id, "userId": user, "actor": actor }));
        Ok(())
    }

    pub fn create_user(&self, name: &str, color: Option<String>, grant: Grant, actor: &str) -> Result<User, UserError> {
        let name = validate_name(name)?;
        let color = validate_color(color)?;
        grant.validate()?;
        let now = crate::management::instance::now_utc();
        let user = User {
            id: format!("{USER_ID_PREFIX}{}", &uuid::Uuid::new_v4().simple().to_string()[..12]),
            name,
            color,
            grant: Some(grant.normalized()),
            created_at: now.clone(),
            updated_at: now,
        };
        self.with(|state| {
            if state.users.len() >= MAX_USERS {
                return Err(UserError::new(409, "too_many_users", format!("at most {MAX_USERS} users")));
            }
            state.users.push(user.clone());
            Ok(())
        })?;
        audit(&self.dir, "userCreated", json!({ "userId": user.id, "actor": actor }));
        Ok(user)
    }

    pub fn update_user(
        &self,
        id: &str,
        name: Option<&str>,
        color: Option<Option<String>>,
        grant: Option<Grant>,
        actor: &str,
    ) -> Result<User, UserError> {
        let name = name.map(validate_name).transpose()?;
        let color = color.map(validate_color).transpose()?;
        if let Some(grant) = &grant {
            grant.validate()?;
        }
        if id == OWNER && grant.is_some() {
            return Err(UserError::new(400, "owner_immutable", "the owner has every right; it has no grant"));
        }
        let user = self.with(|state| {
            let user = state
                .users
                .iter_mut()
                .find(|user| user.id == id)
                .ok_or_else(|| UserError::new(404, "user_not_found", "no user with this id"))?;
            let before = user.clone();
            if let Some(name) = name {
                user.name = name;
            }
            if let Some(color) = color {
                user.color = color;
            }
            if let Some(grant) = grant {
                user.grant = Some(grant.normalized());
            }
            if *user != before {
                user.updated_at = crate::management::instance::now_utc();
            }
            Ok(user.clone())
        })?;
        audit(&self.dir, "userUpdated", json!({ "userId": id, "actor": actor }));
        Ok(user)
    }

    /// Refused while a token or peer is bound to the user.
    pub fn delete_user(&self, id: &str, bound_peers: &[String], actor: &str) -> Result<(), UserError> {
        if id == OWNER {
            return Err(UserError::new(400, "owner_immutable", "the owner cannot be deleted"));
        }
        self.with(|state| {
            if state.user(id).is_none() {
                return Err(UserError::new(404, "user_not_found", "no user with this id"));
            }
            let tokens: Vec<&String> = state
                .bindings
                .iter()
                .filter(|(_, user)| user.as_str() == id)
                .map(|(token, _)| token)
                .collect();
            if !tokens.is_empty() || !bound_peers.is_empty() {
                let mut error = UserError::new(409, "user_in_use", "rebind or revoke this user's devices and peers first");
                error.details = json!({ "tokens": tokens, "peers": bound_peers });
                return Err(error);
            }
            state.users.retain(|user| user.id != id);
            Ok(())
        })?;
        audit(&self.dir, "userDeleted", json!({ "userId": id, "actor": actor }));
        Ok(())
    }

    pub fn set_settings(&self, owner_requires_approval: bool, actor: &str) -> Result<Settings, UserError> {
        let settings = self.with(|state| {
            state.settings.owner_requires_approval = owner_requires_approval;
            Ok(state.settings.clone())
        })?;
        audit(&self.dir, "settingsChanged", json!({ "ownerRequiresApproval": owner_requires_approval, "actor": actor }));
        Ok(settings)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

pub fn principal_in(state: &State, token_id: &str) -> Principal {
    match state.user_of_token(token_id) {
        Some(user) if user.is_owner() => Principal::Owner,
        Some(user) => Principal::User {
            id: user.id.clone(),
            name: user.name.clone(),
            grant: user.grant.clone().unwrap_or_default(),
        },
        None => Principal::Pending,
    }
}

fn validate_name(name: &str) -> Result<String, UserError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS || name.chars().any(char::is_control) {
        return Err(UserError::invalid(format!("name: 1..={MAX_NAME_CHARS} characters, no control characters")));
    }
    Ok(name.to_string())
}

fn validate_color(color: Option<String>) -> Result<Option<String>, UserError> {
    match color {
        None => Ok(None),
        Some(color) => {
            let ok = color.len() == 7
                && color.starts_with('#')
                && color[1..].bytes().all(|b| b.is_ascii_hexdigit());
            if ok { Ok(Some(color.to_ascii_lowercase())) } else { Err(UserError::invalid("color must be #rrggbb")) }
        }
    }
}

/// Append one audit line (best effort). Never holds secrets.
pub fn audit(dir: &Path, action: &str, details: Value) {
    let line = json!({ "ts": crate::management::instance::now_utc(), "action": action, "details": details });
    let result = (|| -> anyhow::Result<()> {
        use std::io::Write;
        crate::management::client_tokens::ensure_private_dir(dir)?;
        let path = dir.join("audit.jsonl");
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        writeln!(file, "{line}")?;
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!("users audit: {error:#}");
    }
}

/// The actor recorded in the audit log for the current request.
pub fn current_actor() -> String {
    match crate::management::listeners::current_client_token() {
        Some(token) => token.token_id,
        None => "root".into(),
    }
}

// ---------------------------------------------------------------- scope

/// What a non-owner user can see right now: local sessions only (a peer's
/// agents are never local sessions, so grants cannot be transitive).
#[derive(Clone, Debug)]
pub struct Scope {
    pub grant: Grant,
    sessions: HashSet<String>,
}

impl Scope {
    pub fn sees_session(&self, cutex_session_id: &str) -> bool {
        self.sessions.contains(cutex_session_id)
    }

    pub fn sees_project(&self, project_id: &str) -> bool {
        match &self.grant.projects {
            Projects::All => true,
            Projects::Only(ids) => ids.iter().any(|id| id == project_id),
        }
    }

    pub fn sessions(&self) -> &HashSet<String> {
        &self.sessions
    }

    /// Visible sessions from the session store and the Agent Management
    /// snapshot: persistent (or IM-exposed), not retired, and in a granted project or listed
    /// as a standalone agent (an agent without a project).
    pub fn load(grant: &Grant) -> anyhow::Result<Self> {
        let snapshot = crate::memory::load_agent_management_snapshot()?;
        let boundary = crate::agent_bus::boundary::AgentBoundary::from_snapshot(
            &crate::config::agent_security::AgentSecurityConfig::default(),
            &snapshot,
        );
        let sessions = crate::session::store::with_cutex_session_store(|store| {
            store
                .sessions
                .values()
                // Persistent agents regardless of the legacy IM flag
                // (`im-exposure-retired`); other sessions only when exposed.
                // Grants and the project boundary below decide the rest.
                .filter(|record| {
                    (record.registration_class
                        == crate::agent_bus::model::AgentRegistrationClass::Persistent
                        || record.exposed_to_backend)
                        && !record.is_retired()
                })
                .filter(|record| {
                    let id = record.cutex_session_id.as_str();
                    match boundary.project_of(id) {
                        Some(project) => match &grant.projects {
                            Projects::All => true,
                            Projects::Only(ids) => ids.iter().any(|p| p == project),
                        },
                        None => grant.standalone_agents.iter().any(|a| a == id),
                    }
                })
                .map(|record| record.cutex_session_id.clone())
                .collect::<HashSet<_>>()
        })?;
        Ok(Self { grant: grant.clone(), sessions })
    }

    /// `load`, cached per grant for a few seconds (peer directory and
    /// stream filters call it often). Errors see nothing.
    pub fn cached(grant: &Grant) -> Self {
        static CACHE: OnceLock<Mutex<Vec<(Grant, Instant, Scope)>>> = OnceLock::new();
        let mut cache = CACHE
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.retain(|(_, at, _)| at.elapsed() < SCOPE_MAX_AGE);
        if let Some((_, _, scope)) = cache.iter().find(|(g, _, _)| g == grant) {
            return scope.clone();
        }
        let scope = Self::load(grant).unwrap_or_else(|error| {
            eprintln!("user scope unavailable; nothing visible: {error:#}");
            Self { grant: grant.clone(), sessions: HashSet::new() }
        });
        cache.push((grant.clone(), Instant::now(), scope.clone()));
        scope
    }

    #[cfg(test)]
    pub fn for_tests(grant: Grant, sessions: &[&str]) -> Self {
        Self { grant, sessions: sessions.iter().map(|s| s.to_string()).collect() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> (PathBuf, UserStore) {
        let root = std::env::temp_dir().join(format!("cutex-users-{}", uuid::Uuid::new_v4()));
        let store = UserStore::at(root.join("users"), ClientTokenStore::at(root.join("clients")));
        (root, store)
    }

    #[test]
    fn migration_binds_every_existing_token_to_owner_and_backs_up() {
        let (root, store) = temp();
        let a = store.tokens().issue("desktop-a").unwrap();
        let (b, _) = store.tokens().issue_device("phone", "phone", "ios").unwrap();
        let state = store.snapshot().unwrap();
        assert_eq!(principal_in(&state, &a.token_id), Principal::Owner);
        assert_eq!(principal_in(&state, &b.token_id), Principal::Owner);
        let migration = state.migration.clone().unwrap();
        assert_eq!(migration.owner_tokens.len(), 2);
        let backup = store.dir().join(migration.backup.unwrap()).join("tokens.json");
        assert!(backup.exists());
        // Tokens issued afterwards are pending until bound.
        let c = store.tokens().issue("desktop-c").unwrap();
        let state = store.snapshot().unwrap();
        assert_eq!(principal_in(&state, &c.token_id), Principal::Pending);
        let raw = std::fs::read_to_string(store.path()).unwrap();
        assert!(!raw.contains(&a.token) && !raw.contains("cxm1."), "no secrets in the users store");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn first_device_is_owner_then_pending_unless_approval_is_off() {
        let (root, store) = temp();
        assert_eq!(store.default_cli_binding().unwrap().as_deref(), Some(OWNER));
        // Any existing device (even a pending one) ends the first-pairing exception.
        let first = store.tokens().issue("first").unwrap();
        assert_eq!(store.default_cli_binding().unwrap(), None);
        store.bind_token(&first.token_id, Some(OWNER), "root").unwrap();
        assert_eq!(store.default_cli_binding().unwrap(), None);
        store.set_settings(false, "root").unwrap();
        assert_eq!(store.default_cli_binding().unwrap().as_deref(), Some(OWNER));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn users_grants_bindings_and_guards() {
        let (root, store) = temp();
        let token = store.tokens().issue("c").unwrap();
        let user = store
            .create_user("用户 2", Some("#AABBCC".into()), Grant { projects: Projects::Only(vec!["p2".into(), "p1".into(), "p1".into()]), ..Grant::default() }, "root")
            .unwrap();
        assert!(valid_user_id(&user.id));
        assert_eq!(user.color.as_deref(), Some("#aabbcc"));
        assert_eq!(user.grant.as_ref().unwrap().projects, Projects::Only(vec!["p1".into(), "p2".into()]));
        store.bind_token(&token.token_id, Some(&user.id), "root").unwrap();
        let state = store.snapshot().unwrap();
        match principal_in(&state, &token.token_id) {
            Principal::User { id, grant, .. } => {
                assert_eq!(id, user.id);
                assert!(!grant.send);
            }
            other => panic!("unexpected {other:?}"),
        }
        // Owner cannot get a grant nor be deleted; bound users cannot be deleted.
        assert_eq!(store.update_user(OWNER, None, None, Some(Grant::default()), "root").unwrap_err().code, "owner_immutable");
        assert_eq!(store.delete_user(OWNER, &[], "root").unwrap_err().code, "owner_immutable");
        let busy = store.delete_user(&user.id, &[], "root").unwrap_err();
        assert_eq!(busy.code, "user_in_use");
        assert_eq!(busy.details["tokens"][0], token.token_id);
        assert_eq!(store.delete_user(&user.id, &["peer-1".into()], "root").unwrap_err().code, "user_in_use");
        // Unknown users and tokens.
        assert_eq!(store.bind_token(&token.token_id, Some("u-000000000000"), "root").unwrap_err().code, "user_not_found");
        assert_eq!(store.bind_token("mct-nope", Some(OWNER), "root").unwrap_err().code, "client_token_not_found");
        // Unbinding makes the token pending; then the user can be deleted.
        store.bind_token(&token.token_id, None, "root").unwrap();
        store.delete_user(&user.id, &[], "root").unwrap();
        let state = store.snapshot().unwrap();
        assert_eq!(principal_in(&state, &token.token_id), Principal::Pending);
        // Revoked tokens lose their binding on the next write.
        store.bind_token(&token.token_id, Some(OWNER), "root").unwrap();
        store.tokens().revoke(&token.token_id).unwrap();
        store.set_settings(true, "root").unwrap();
        assert!(!store.snapshot().unwrap().bindings.contains_key(&token.token_id));
        // Validation.
        assert!(store.create_user(" ", None, Grant::default(), "root").is_err());
        assert!(store.create_user("x", Some("red".into()), Grant::default(), "root").is_err());
        assert!(store
            .create_user("x", None, Grant { standalone_agents: vec!["a b".into()], ..Grant::default() }, "root")
            .is_err());
        let audit = std::fs::read_to_string(store.dir().join("audit.jsonl")).unwrap();
        assert!(audit.contains("userCreated") && !audit.contains("cxm1."));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_binding_to_a_deleted_user_is_pending_and_corrupt_stores_fail_closed() {
        let (root, store) = temp();
        let token = store.tokens().issue("c").unwrap();
        store.snapshot().unwrap();
        let mut raw: Value = serde_json::from_slice(&std::fs::read(store.path()).unwrap()).unwrap();
        raw["bindings"][&token.token_id] = json!("u-0123456789ab");
        std::fs::write(store.path(), serde_json::to_vec(&raw).unwrap()).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(store.principal_for_token(&token.token_id), Principal::Pending);
        std::fs::write(store.path(), b"{not json").unwrap();
        assert_eq!(store.principal_for_token(&token.token_id), Principal::Pending);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn grant_json_shape() {
        let all: Grant = serde_json::from_value(json!({"projects": "all", "send": true})).unwrap();
        assert_eq!(all.projects, Projects::All);
        assert_eq!(serde_json::to_value(&all).unwrap(), json!({"projects": "all", "standaloneAgents": [], "send": true}));
        assert!(serde_json::from_value::<Grant>(json!({"projects": "some"})).is_err());
        assert!(serde_json::from_value::<Grant>(json!({"projects": [], "extra": 1})).is_err());
        assert_eq!(Grant::default().projects, Projects::Only(vec![]));
        let scope = Scope::for_tests(Grant::default(), &["s1"]);
        assert!(scope.sees_session("s1") && !scope.sees_session("s2") && !scope.sees_project("p"));
    }
}
