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

/// A user's role in one project (`projectRoles`, capability
/// `user-project-roles`). `viewer`: the project's agents, their history,
/// summaries, workspace files and artifacts, read-only. `collaborator`:
/// viewer, and send to the project's agents under the scoped send rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectRole {
    Viewer,
    Collaborator,
}

impl ProjectRole {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "viewer" => Some(Self::Viewer),
            "collaborator" => Some(Self::Collaborator),
            _ => None,
        }
    }
}

/// What a non-owner user may see and do. Default: nothing, read-only.
///
/// `projects` / `standaloneAgents` make agents visible and `send` lets
/// the user send to every visible agent. `projectRoles` grants single
/// projects with their own role; a project with a role follows that role
/// for sending (a `viewer` project is read-only even with `send: true`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Grant {
    #[serde(default)]
    pub projects: Projects,
    #[serde(default)]
    pub standalone_agents: Vec<String>,
    #[serde(default)]
    pub send: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub project_roles: BTreeMap<String, ProjectRole>,
}

impl Grant {
    pub fn validate(&self) -> Result<(), UserError> {
        if let Projects::Only(ids) = &self.projects {
            validate_ids(ids, "projects")?;
        }
        let roles: Vec<String> = self.project_roles.keys().cloned().collect();
        validate_ids(&roles, "projectRoles")?;
        validate_ids(&self.standalone_agents, "standaloneAgents")
    }

    /// The project is visible: listed in `projects` (or `all`) or given a role.
    pub fn sees_project(&self, project_id: &str) -> bool {
        self.project_roles.contains_key(project_id)
            || match &self.projects {
                Projects::All => true,
                Projects::Only(ids) => ids.iter().any(|id| id == project_id),
            }
    }

    /// May send to a visible agent of `project` (`None`: a standalone agent).
    pub fn sends_in(&self, project: Option<&str>) -> bool {
        match project.and_then(|project| self.project_roles.get(project)) {
            Some(role) => *role == ProjectRole::Collaborator,
            None => self.send,
        }
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

/// A token that became an owner device (owner-device-notice; cutex-desktop
/// docs/research/host-ownership-20261019.md §5 limit 1) and that no owner
/// principal has acknowledged yet. Dropped when the token is revoked or
/// leaves the owner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnerDeviceNotice {
    pub token_id: String,
    /// When it was bound to the owner.
    pub at: String,
    /// `pairing` (an owner pairing code), `assigned` (the owner assigned a
    /// pending device), `personKey` (a pending device claimed a person key
    /// of the owner), `ssh` (issued or assigned with the CLI on the host).
    pub via: String,
    /// The host event `cutex/access/ownerDeviceAdded` was published.
    #[serde(default)]
    pub announced: bool,
}

pub const OWNER_DEVICE_ADDED: &str = "cutex/access/ownerDeviceAdded";
pub const VIA_PAIRING: &str = "pairing";
const VIA_PAIRING_V2_APPROVED: &str = "pairing-v2-approved";
pub const VIA_ASSIGNED: &str = "assigned";
pub const VIA_PERSON_KEY: &str = "personKey";
pub const VIA_SSH: &str = "ssh";
const MAX_OWNER_DEVICE_NOTICES: usize = 100;

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
    /// Unacknowledged new owner devices (older builds ignore the field and
    /// drop it on their next write).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub new_owner_devices: Vec<OwnerDeviceNotice>,
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
            new_owner_devices: Vec::new(),
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

/// Owner powers on the current request: the owner, or a direct root / bridge
/// caller with no user principal. A shared-access user's request carries a
/// Human principal for its granted routes, but that is not owner proof (the
/// client-token bearer is normalized to the root credential).
pub fn caller_is_owner() -> bool {
    current_principal().is_none_or(|principal| principal.is_owner())
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
    /// A token newly bound to `owner` is recorded as a new owner device with
    /// `via` [`VIA_ASSIGNED`]; see [`Self::with_via`].
    pub fn with<T>(
        &self,
        mutate: impl FnOnce(&mut State) -> Result<T, UserError>,
    ) -> Result<T, UserError> {
        self.with_via(VIA_ASSIGNED, mutate)
    }

    /// [`Self::with`], recording every token that `mutate` newly bound to
    /// `owner` as an unacknowledged new owner device reached `via` (one
    /// place for every path: pairing, assignment, person-key auto-binding,
    /// the CLI). The migration's bindings are not new devices.
    pub fn with_via<T>(
        &self,
        via: &str,
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
        record_new_owner_devices(&before, &mut state, via);
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

    /// Acknowledge a new owner device notice (`POST
    /// /v2/users/owner/new-devices/{tokenId}/ack`). `Ok(false)` when there was
    /// none (idempotent).
    pub fn ack_owner_device(&self, token_id: &str, actor: &str) -> Result<bool, UserError> {
        let acked = self.with(|state| {
            let before = state.new_owner_devices.len();
            state.new_owner_devices.retain(|notice| notice.token_id != token_id);
            Ok(state.new_owner_devices.len() != before)
        })?;
        if acked {
            audit(&self.dir, "ownerDeviceAcknowledged", json!({ "tokenId": token_id, "actor": actor }));
        }
        Ok(acked)
    }

    /// Mark every not yet announced new owner device as announced and return
    /// them (the caller publishes `cutex/access/ownerDeviceAdded`). Cheap
    /// when there is nothing to announce (cached snapshot).
    pub fn take_unannounced_owner_devices(&self) -> Vec<OwnerDeviceNotice> {
        // Never create (migrate) the store just to look.
        if !self.path().exists() {
            return Vec::new();
        }
        let pending = self
            .snapshot()
            .is_ok_and(|state| state.new_owner_devices.iter().any(|notice| !notice.announced));
        if !pending {
            return Vec::new();
        }
        self.with(|state| {
            let mut taken = Vec::new();
            for notice in &mut state.new_owner_devices {
                if !notice.announced {
                    notice.announced = true;
                    taken.push(notice.clone());
                }
            }
            Ok(taken)
        })
        .unwrap_or_default()
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

    /// Bind (`Some(user)`) or unbind (`None` = pending) a token. A new owner
    /// binding is recorded as a new owner device: `via` `pairing` for the
    /// actor `pairing-code`, `ssh` for `cli`, else `assigned`.
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
        let via = match actor {
            "pairing-code" => VIA_PAIRING,
            "pairing-v2-approved" => VIA_PAIRING_V2_APPROVED,
            "cli" => VIA_SSH,
            _ => VIA_ASSIGNED,
        };
        self.with_via(via, |state| {
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

    /// Set (`Some`) or remove (`None`) one project's role for a user
    /// (owner only, through the access gate). Removing also drops the
    /// project from an explicit `projects` list, so the project is no longer
    /// visible (`projects: "all"` stays as it is). Returns the user and
    /// whether anything changed.
    pub fn set_project_role(
        &self,
        id: &str,
        project_id: &str,
        role: Option<ProjectRole>,
        actor: &str,
    ) -> Result<(User, bool), UserError> {
        if id == OWNER {
            return Err(UserError::new(400, "owner_immutable", "the owner has every right; it has no grant"));
        }
        validate_ids(&[project_id.to_string()], "projectId")?;
        let (user, changed) = self.with(|state| {
            let user = state
                .users
                .iter_mut()
                .find(|user| user.id == id)
                .ok_or_else(|| UserError::new(404, "user_not_found", "no user with this id"))?;
            let mut grant = user.grant.clone().unwrap_or_default();
            match role {
                Some(role) => {
                    grant.project_roles.insert(project_id.to_string(), role);
                }
                None => {
                    grant.project_roles.remove(project_id);
                    if let Projects::Only(ids) = &mut grant.projects {
                        ids.retain(|project| project != project_id);
                    }
                }
            }
            grant.validate()?;
            let grant = grant.normalized();
            let changed = user.grant.as_ref() != Some(&grant);
            if changed {
                user.grant = Some(grant);
                user.updated_at = crate::management::instance::now_utc();
            }
            Ok((user.clone(), changed))
        })?;
        if changed {
            audit(&self.dir, "userProjectRoleChanged", json!({
                "userId": id, "projectId": project_id, "role": role, "actor": actor,
            }));
        }
        Ok((user, changed))
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
        // The users lock has been released before entering the layout store.
        crate::management::user_layout::LayoutStore::at(&self.dir).remove(id)?;
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

/// Tokens bound to `owner` in `after` but not in `before` become new owner
/// device notices; notices of tokens no longer bound to `owner` go.
fn record_new_owner_devices(before: &State, after: &mut State, via: &str) {
    let now = crate::management::instance::now_utc();
    let added: Vec<String> = after
        .bindings
        .iter()
        .filter(|(token, user)| user.as_str() == OWNER && before.bindings.get(*token).map(String::as_str) != Some(OWNER))
        .map(|(token, _)| token.clone())
        .collect();
    for token in added {
        after.new_owner_devices.retain(|notice| notice.token_id != token);
        if via == VIA_PAIRING_V2_APPROVED { continue; }
        after.new_owner_devices.push(OwnerDeviceNotice { token_id: token, at: now.clone(), via: via.to_string(), announced: false });
    }
    let bindings = &after.bindings;
    after.new_owner_devices.retain(|notice| bindings.get(&notice.token_id).map(String::as_str) == Some(OWNER));
    let excess = after.new_owner_devices.len().saturating_sub(MAX_OWNER_DEVICE_NOTICES);
    after.new_owner_devices.drain(..excess);
}

/// Publish `cutex/access/ownerDeviceAdded {tokenId, label, deviceName,
/// platform, scope, at, via}` on the host event stream for every new owner
/// device not announced yet (daemon only: the CLI never writes the event
/// log; a device the CLI added is announced by the daemon's next request).
pub fn announce_owner_devices(store: &UserStore) {
    let notices = store.take_unannounced_owner_devices();
    if notices.is_empty() {
        return;
    }
    let tokens = store.tokens.list().unwrap_or_default();
    for notice in notices {
        let token = tokens.iter().find(|token| token.token_id == notice.token_id);
        crate::management::v2::documents::publish_host_event(
            OWNER_DEVICE_ADDED,
            json!({
                "tokenId": notice.token_id,
                "label": token.map(|t| t.label.clone()),
                "deviceName": token.and_then(|t| t.device_name.clone()),
                "platform": token.and_then(|t| t.platform.clone()),
                "scope": token.map(|t| t.scope),
                "at": notice.at,
                "via": notice.via,
            }),
        );
    }
}

/// The owner-local rule (owner-local-only; cutex-desktop
/// docs/research/host-ownership-20261019.md §5 limit 2, "本机和 SSH 都算"):
/// a loopback listener AND either the root credential (the desktop on the
/// host, the CLI) or a client token that is bound to the owner AND was
/// issued on the host by `cutex management client-token issue` (someone
/// with the host's OS account; an SSH-tunnelled desktop). A token from a
/// pairing code never counts, owner-bound or not, loopback or not; nor does
/// any remote listener. The device scope still applies on top (a narrowed
/// owner-local token is refused by the scope gate first).
pub fn request_is_owner_local() -> bool {
    if !crate::management::listeners::current_request_is_loopback() {
        return false;
    }
    match crate::management::listeners::current_client_token() {
        None => true,
        Some(token) => {
            token.principal.is_owner()
                && token.issued_via == crate::management::client_tokens::IssuedVia::HostCli
        }
    }
}

/// Reading credentials that grant control of the host must also respect the
/// token's scope. GET requests otherwise pass the read/chat device gate.
pub fn request_has_owner_local_secret_access() -> bool {
    request_is_owner_local()
        && crate::management::listeners::current_client_token().is_none_or(|token| token.scope.is_full())
}

/// 403 `owner_local_only` for a root-identity action (owner-local-only;
/// cutex-desktop docs/research/host-ownership-20261019.md §5 limit 2).
pub fn owner_local_only(action: &str) -> UserError {
    let mut error = UserError::new(
        403,
        "owner_local_only",
        "this action changes who controls the host: only on the host itself or over SSH (the root credential, or an owner token issued with `cutex management client-token issue`, on a loopback listener) or with the cutex CLI there",
    );
    error.details = owner_local_details(json!({ "action": action }));
    error
}

/// `details` of an owner-local refusal: `{…extra, clientToken, issuedVia}`
/// (`issuedVia` null for the root credential).
pub fn owner_local_details(mut extra: Value) -> Value {
    let token = crate::management::listeners::current_client_token();
    extra["clientToken"] = json!(token.is_some());
    extra["issuedVia"] = json!(token.map(|token| token.issued_via));
    extra
}

// r290 P10 (B6) ---------------------------------------- owner device actions
//
// The owner-local rule above still decides who may act without asking. On
// top of it, a FULL-scope owner device on any listener (LAN / tailnet /
// relay, or a paired token on loopback) may:
// - always TURN OFF / remove / revoke (`OWNER_TURN_OFF_ACTIONS`): these never
//   widen anyone's access, and that device already has a host terminal;
// - WIDEN (`OWNER_WIDENING_ACTIONS`) only with a fresh, explicit confirm:
//   the first call answers 428 `owner_confirm_required` with what will
//   happen and a single-use `confirmToken` (bound to this token and action,
//   2 minutes); the same request repeated with `?ownerConfirm=<token>` runs
//   and every owner device is told (host event
//   `cutex/access/ownerRemoteAction`, users audit `ownerRemoteAction`).
// Narrowed (read / chat) tokens, other users' and pending tokens stay
// refused (403 `owner_local_only`, or earlier by the scope / user gates).

/// Actions that only turn something off, remove or revoke.
pub const OWNER_TURN_OFF_ACTIONS: &[&str] = &[
    "peerDisable",              // PATCH /v2/peers/{id} {enabled:false}
    "peerRemove",               // DELETE /v2/peers/{id}
    "peersDisable",             // PATCH /v2/settings {peers:{enabled:false}}
    "publicPairingOff",         // POST /v2/relay/pairing {enabled:false}
    "relayCertificateRevoke",   // DELETE /v2/relay/certificates/{id}
    "pairingInviteCancel",      // DELETE /v2/pairing/invites/{id}, POST …/{id}/deny
    "deviceRevoke",             // DELETE /v2/client-tokens/{id} (already open to owner devices)
    "relayCertificateRenewOwn", // POST /v2/relay-certificate/renew: the device's own key, incl. grace
];

/// Actions that hand out or widen access.
pub const OWNER_WIDENING_ACTIONS: &[&str] = &[
    "pairingCode",             // POST /v2/pairing-codes (still loopback listener only: v1 codes)
    "pairingInvite",           // POST /v2/pairing/invites (QR v2: a new device)
    "pairingInviteApprove",    // POST /v2/pairing/invites/{id}/approve
    "relayCertificateApprove", // POST /v2/relay/certificates (enrollment approval)
    "peerPair",                // POST /v2/peers, POST /v2/peers/invites
    "peerEnable",              // PATCH /v2/peers/{id} {enabled:true}
    "peersEnable",             // PATCH /v2/settings {peers:{enabled:true}}
    "publicPairingOn",         // POST /v2/relay/pairing {enabled:true}
    "disableOwnerApproval",    // PATCH /v2/users/settings {ownerRequiresApproval:false}
];

/// Host event every owner device sees after a confirmed remote widening action.
pub const OWNER_REMOTE_ACTION: &str = "cutex/access/ownerRemoteAction";
const OWNER_CONFIRM_TTL: Duration = Duration::from_secs(120);
const MAX_OWNER_CONFIRMS: usize = 64;

struct OwnerConfirm {
    action: String,
    token_id: String,
    request_digest: String,
    expires: Instant,
}

fn owner_confirms() -> &'static Mutex<BTreeMap<String, OwnerConfirm>> {
    static CONFIRMS: OnceLock<Mutex<BTreeMap<String, OwnerConfirm>>> = OnceLock::new();
    CONFIRMS.get_or_init(Default::default)
}

/// A full-scope token bound to the owner (any listener, any origin).
pub fn request_is_full_owner_device() -> bool {
    crate::management::listeners::current_client_token()
        .is_some_and(|token| token.principal.is_owner() && token.scope.is_full())
}

/// May the current request run a turning-off action (`OWNER_TURN_OFF_ACTIONS`)?
pub fn owner_may_turn_off() -> bool {
    request_is_owner_local() || request_is_full_owner_device()
}

/// `?ownerConfirm=<token>` of a request path.
pub fn owner_confirm_param(path: &str) -> Option<&str> {
    path.split_once('?')?
        .1
        .split('&')
        .find_map(|pair| pair.strip_prefix("ownerConfirm="))
        .filter(|value| !value.is_empty())
}

fn owner_action_summary(action: &str) -> &'static str {
    match action {
        "pairingCode" => "create a pairing code: whoever redeems it gets a new device token",
        "pairingInvite" => "create a QR pairing invite: the scanned phone becomes a new device",
        "pairingInviteApprove" => "approve a pending phone: it gets a device token and a relay certificate",
        "relayCertificateApprove" => "issue a relay certificate to a device",
        "peerPair" => "pair this host with another host",
        "peerEnable" => "enable a paired host",
        "peersEnable" => "turn cross-host peers on",
        "publicPairingOn" => "turn public pairing (the relay bootstrap endpoint) on",
        "disableOwnerApproval" => "let owner devices act without the owner's approval",
        _ => "widen access to this host",
    }
}

/// Gate of a widening action (`OWNER_WIDENING_ACTIONS`). Owner-local: Ok as
/// before. A full-scope owner device: Ok with a valid `?ownerConfirm=`
/// (consumed; every owner device is told), else 428
/// `owner_confirm_required {action, summary, confirmToken, expiresAt,
/// ttlSeconds, clientToken, issuedVia}`. Anyone else: 403 `owner_local_only`.
pub fn owner_widening_gate(action: &str, request: &crate::http::server::SimpleHttpRequest) -> Result<(), UserError> {
    if request_is_owner_local() {
        return Ok(());
    }
    let token = match crate::management::listeners::current_client_token() {
        Some(token) if token.principal.is_owner() && token.scope.is_full() => token,
        _ => return Err(owner_local_only(action)),
    };
    // Bind confirmation to the exact method, target, query and body. Only
    // the confirmation parameter itself may change between the two clicks.
    let (path, query) = request.path.split_once('?').unwrap_or((&request.path, ""));
    let query: Vec<_> = url::form_urlencoded::parse(query.as_bytes())
        .filter(|(name, _)| name != "ownerConfirm").collect();
    let request_digest = crate::approvals::request_digest(&json!({
        "method": request.method, "path": path, "query": query, "body": request.body,
    }));
    let now = Instant::now();
    let mut confirms = owner_confirms().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    confirms.retain(|_, confirm| confirm.expires > now);
    if let Some(presented) = owner_confirm_param(&request.path) {
        let valid = confirms
            .get(presented)
            .is_some_and(|confirm| confirm.action == action && confirm.token_id == token.token_id && confirm.request_digest == request_digest);
        if valid {
            confirms.remove(presented);
            drop(confirms);
            announce_owner_remote_action(action, &token);
            return Ok(());
        }
    }
    if confirms.len() >= MAX_OWNER_CONFIRMS {
        let oldest = confirms.iter().min_by_key(|(_, confirm)| confirm.expires).map(|(id, _)| id.clone());
        if let Some(oldest) = oldest {
            confirms.remove(&oldest);
        }
    }
    let confirm_token = format!("oc-{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
    confirms.insert(
        confirm_token.clone(),
        OwnerConfirm { action: action.to_string(), token_id: token.token_id.clone(), request_digest, expires: now + OWNER_CONFIRM_TTL },
    );
    let mut error = UserError::new(
        428,
        "owner_confirm_required",
        format!(
            "{}: confirm on this device, then repeat the request with ?ownerConfirm=<confirmToken>; every owner device will be told",
            owner_action_summary(action)
        ),
    );
    let expires_at = chrono::Utc::now()
        + chrono::Duration::from_std(OWNER_CONFIRM_TTL).unwrap_or_default();
    error.details = owner_local_details(json!({
        "action": action,
        "summary": owner_action_summary(action),
        "confirmToken": confirm_token,
        "confirmParam": "ownerConfirm",
        "expiresAt": expires_at.to_rfc3339(),
        "ttlSeconds": OWNER_CONFIRM_TTL.as_secs(),
    }));
    Err(error)
}

/// Tell every owner device about a confirmed widening action from a device.
fn announce_owner_remote_action(action: &str, token: &crate::management::client_tokens::VerifiedClientToken) {
    let summary = ClientTokenStore::open_default()
        .ok()
        .and_then(|store| store.list().ok())
        .and_then(|tokens| tokens.into_iter().find(|t| t.token_id == token.token_id));
    let connection = if crate::management::relay::current_peer().is_some() {
        "relay"
    } else if crate::management::listeners::current_request_is_loopback() {
        "loopback"
    } else {
        "remote"
    };
    let details = json!({
        "action": action,
        "summary": owner_action_summary(action),
        "phase": "confirmed",
        "tokenId": token.token_id,
        "label": summary.as_ref().map(|t| t.label.clone()),
        "deviceName": summary.as_ref().and_then(|t| t.device_name.clone()),
        "platform": summary.as_ref().and_then(|t| t.platform.clone()),
        "connection": connection,
        "at": crate::management::instance::now_utc(),
    });
    if let Ok(store) = UserStore::open_default() {
        audit(store.dir(), "ownerRemoteAction", details.clone());
    }
    crate::management::v2::documents::publish_host_event(OWNER_REMOTE_ACTION, details);
}

/// Status line reason of the statuses users errors use.
pub fn reason_phrase(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        428 => "Precondition Required",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Service Unavailable",
    }
}
// r290 P10 (B6) end

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

/// The shared snapshots a [`Scope`] is computed from: the Agent Management
/// directory (project membership) and the session store.
struct Membership(
    Arc<crate::agent_management::AgentManagementSnapshot>,
    Arc<crate::session::model::CutexSessionStore>,
);

impl Membership {
    fn current() -> anyhow::Result<Self> {
        let directory = crate::agent_management::AgentManagementStore::open_default()?
            .directory_snapshot_shared()
            .map_err(anyhow::Error::new)?;
        Ok(Self(directory, crate::session::store::shared_cutex_session_store()?))
    }

    /// Both stores' commit revisions: every committed change bumps one.
    /// (Only the numbers are kept: holding a session snapshot would make
    /// the next commit copy the whole store.)
    fn revisions(&self) -> (u64, u64) {
        (self.0.store_revision, self.1.store_revision.get())
    }
}

/// What a non-owner user can see right now: local sessions only (a peer's
/// agents are never local sessions, so grants cannot be transitive).
#[derive(Clone, Debug)]
pub struct Scope {
    pub grant: Grant,
    sessions: HashSet<String>,
    /// Visible sessions the user may send to.
    send_sessions: HashSet<String>,
    /// Visible local projects (known to Agent Management, or named in the grant).
    projects: HashSet<String>,
}

impl Scope {
    pub fn sees_session(&self, cutex_session_id: &str) -> bool {
        self.sessions.contains(cutex_session_id)
    }

    pub fn sees_project(&self, project_id: &str) -> bool {
        self.grant.sees_project(project_id)
    }

    /// May send to this visible agent (`send`, or its project's
    /// `collaborator` role; a `viewer` project is read-only).
    pub fn may_send(&self, cutex_session_id: &str) -> bool {
        self.send_sessions.contains(cutex_session_id)
    }

    /// Visible local projects.
    pub fn projects(&self) -> &HashSet<String> {
        &self.projects
    }

    pub fn sessions(&self) -> &HashSet<String> {
        &self.sessions
    }

    /// Visible sessions from the session store and the Agent Management
    /// snapshot: persistent (or IM-exposed), not retired, and in a granted project or listed
    /// as a standalone agent (an agent without a project).
    pub fn load(grant: &Grant) -> anyhow::Result<Self> {
        let Membership(snapshot, store) = Membership::current()?;
        Ok(Self::load_from(grant, &snapshot, &store))
    }

    fn load_from(
        grant: &Grant,
        snapshot: &crate::agent_management::AgentManagementSnapshot,
        store: &crate::session::model::CutexSessionStore,
    ) -> Self {
        let boundary = crate::agent_bus::boundary::AgentBoundary::from_snapshot(
            &crate::config::agent_security::AgentSecurityConfig::default(),
            snapshot,
        );
        let visible = {
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
                .filter_map(|record| {
                    let id = record.cutex_session_id.as_str();
                    let project = boundary.project_of(id);
                    let visible = match project {
                        Some(project) => grant.sees_project(project),
                        None => grant.standalone_agents.iter().any(|a| a == id),
                    };
                    visible.then(|| (record.cutex_session_id.clone(), grant.sends_in(project)))
                })
                .collect::<Vec<_>>()
        };
        let send_sessions = visible.iter().filter(|(_, send)| *send).map(|(id, _)| id.clone()).collect();
        let sessions = visible.into_iter().map(|(id, _)| id).collect();
        let mut projects: HashSet<String> = snapshot
            .projects
            .keys()
            .map(|project| project.as_str().to_string())
            .filter(|project| grant.sees_project(project))
            .collect();
        if let Projects::Only(ids) = &grant.projects {
            projects.extend(ids.iter().cloned());
        }
        projects.extend(grant.project_roles.keys().cloned());
        Self { grant: grant.clone(), sessions, send_sessions, projects }
    }

    /// `load`, cached per grant and membership (peer directory and stream
    /// filters call it often). r290 P1: an entry is reused only while the
    /// Agent Management directory and the session store are the same
    /// snapshots it was built from, so a project move, retirement or new
    /// agent takes effect at once; entries also expire after a few seconds.
    /// Errors see nothing.
    pub fn cached(grant: &Grant) -> Self {
        static CACHE: OnceLock<Mutex<Vec<(Grant, Instant, (u64, u64), Scope)>>> = OnceLock::new();
        let empty = || Self { grant: grant.clone(), sessions: HashSet::new(), send_sessions: HashSet::new(), projects: HashSet::new() };
        let membership = match Membership::current() {
            Ok(membership) => membership,
            Err(error) => {
                eprintln!("user scope unavailable; nothing visible: {error:#}");
                return empty();
            }
        };
        let mut cache = CACHE
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let revisions = membership.revisions();
        cache.retain(|(_, at, held, _)| at.elapsed() < SCOPE_MAX_AGE && *held == revisions);
        if let Some((_, _, _, scope)) = cache.iter().find(|(g, _, _, _)| g == grant) {
            return scope.clone();
        }
        let scope = Self::load_from(grant, &membership.0, &membership.1);
        cache.push((grant.clone(), Instant::now(), revisions, scope.clone()));
        scope
    }

    #[cfg(test)]
    pub fn for_tests(grant: Grant, sessions: &[&str]) -> Self {
        let sessions: HashSet<String> = sessions.iter().map(|s| s.to_string()).collect();
        let send_sessions = if grant.send { sessions.clone() } else { HashSet::new() };
        let mut projects: HashSet<String> = grant.project_roles.keys().cloned().collect();
        if let Projects::Only(ids) = &grant.projects {
            projects.extend(ids.iter().cloned());
        }
        Self { grant, sessions, send_sessions, projects }
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
    fn v2_explicit_allow_suppresses_only_its_own_new_owner_notice() {
        let (root,store)=temp();store.snapshot().unwrap();
        for (actor,notice) in [("pairing-v2-approved",false),("pairing-code",true),("cli",true),("root",true)] {
            let issued=store.tokens().issue(actor).unwrap();store.bind_token(&issued.token_id,Some(OWNER),actor).unwrap();
            assert_eq!(store.snapshot().unwrap().new_owner_devices.iter().any(|n|n.token_id==issued.token_id),notice);
        }
        assert_eq!(store.snapshot().unwrap().new_owner_devices.len(),3);
        let _=std::fs::remove_dir_all(root);
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
