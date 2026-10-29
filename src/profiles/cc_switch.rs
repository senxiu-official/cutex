//! Read-only reader of CC Switch's provider store (farion1231/cc-switch, MIT),
//! for `GET|POST /v2/profile-imports/cc-switch` (capability
//! `profile-import-cc-switch`).
//!
//! CC Switch keeps its data in `<home>/.cc-switch/` on every OS (the real
//! user home, not cutex's instance home). The owner may move it: the Tauri
//! store `app_paths.json` (key `app_config_dir_override`) in the app-data
//! directory of `com.ccswitch.desktop` names the moved directory.
//!
//! - v3.8+: SQLite `cc-switch.db`, audited through `PRAGMA user_version`
//!   [`KNOWN_SCHEMA_VERSION`]; newer versions are read by known column names
//!   while required provider fields remain available. Table `providers` is
//!   keyed by `(id, app_type)`. Opened read-only (no WAL / journal files created).
//! - before 3.8: `config.json` `{"version": 2, "claude": {"providers": {id:
//!   Provider}, "current": id}, "codex": {...}}`.
//!
//! The current provider of each app is `settings.json`
//! `currentProviderClaude` / `currentProviderCodex`, else `is_current` (db) /
//! `current` (json).
//!
//! CC Switch stores keys in plaintext. A key read here only ever lives in a
//! [`Secret`] (redacted `Debug`); nothing derived from it (errors, listings)
//! carries it.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

/// Newest audited `PRAGMA user_version`, not a ceiling for additive changes.
/// Upstream de0a235ad6389ce4afa333b472cdee615a3f3987 schema 21..23 only
/// adds MCP/Skills enable flags; the provider fields read here are unchanged.
pub const KNOWN_SCHEMA_VERSION: i64 = 23;
pub const DIR_NAME: &str = ".cc-switch";
pub const DB_FILE: &str = "cc-switch.db";
pub const JSON_FILE: &str = "config.json";
pub const SETTINGS_FILE: &str = "settings.json";
/// Tauri app identifier (its app-data directory holds `app_paths.json`).
pub const APP_IDENTIFIER: &str = "com.ccswitch.desktop";
const APP_PATHS_FILE: &str = "app_paths.json";
const OVERRIDE_KEY: &str = "app_config_dir_override";
const BUSY_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_JSON_BYTES: u64 = 32 * 1024 * 1024;
const MAX_SMALL_JSON_BYTES: u64 = 1024 * 1024;
const MAX_PROVIDERS: usize = 2000;
/// `meta.providerType` values that log in with OAuth (no key to import).
const OAUTH_PROVIDER_TYPES: &[&str] = &["github_copilot", "codex_oauth", "xai_oauth"];
/// Variables that can carry a Claude key.
const CLAUDE_KEY_VARS: [&str; 2] = ["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY"];

/// A key read from CC Switch. `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum App {
    Claude,
    Codex,
}

impl App {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Sqlite,
    Json,
}

impl Format {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sqlite => "sqlite",
            Self::Json => "json",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub path: PathBuf,
    pub format: Format,
    /// `PRAGMA user_version` (sqlite) / `version` (json).
    pub schema_version: Option<i64>,
}

/// Which variable carries a Claude key (`meta.apiKeyField`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaudeKeyVar {
    AuthToken,
    ApiKey,
}

#[derive(Clone, Debug)]
pub struct Provider {
    pub id: String,
    pub app: App,
    pub name: String,
    /// Display base URL (never with credentials, query or fragment).
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub current: bool,
    pub key: Option<Secret>,
    /// `None` = importable; else why not (English, secret-free).
    pub unsupported: Option<String>,
    /// Claude: the variable that carried the key.
    pub claude_key_var: ClaudeKeyVar,
    /// Claude: non-secret Claude Code settings cutex Claude API accounts
    /// accept (model aliases etc.).
    pub claude_env: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanError {
    /// `unsupported_version` | `unsupported_format` | `unreadable`.
    pub code: &'static str,
    pub message: String,
}

#[derive(Clone, Debug, Default)]
pub struct Scan {
    pub found: bool,
    pub source: Option<Source>,
    pub error: Option<ScanError>,
    pub providers: Vec<Provider>,
}

impl Scan {
    pub fn find(&self, id: &str, app: App) -> Option<&Provider> {
        self.providers.iter().find(|provider| provider.id == id && provider.app == app)
    }
}

/// Where to look: the user's home and the OS app-data directory.
#[derive(Clone, Debug)]
pub struct Locations {
    pub home: PathBuf,
    pub app_data: Option<PathBuf>,
}

impl Locations {
    /// The real user home (`dirs::home_dir`) and app-data dir
    /// (`dirs::data_dir`: `~/.local/share`, `~/Library/Application Support`,
    /// `%APPDATA%`).
    #[cfg(not(test))]
    pub fn current() -> Option<Self> {
        Some(Self { home: dirs::home_dir()?, app_data: dirs::data_dir() })
    }

    /// Tests never see the real profile (`dirs::home_dir` ignores `HOME` on
    /// Windows): the locations a test set with [`tests::LocationsOverride`],
    /// else the verified private test home (marker, not the real profile),
    /// else none.
    #[cfg(test)]
    pub fn current() -> Option<Self> {
        if let Some(locations) = tests::LOCATIONS_OVERRIDE.with(|cell| cell.borrow().clone()) {
            return Some(locations);
        }
        let home = crate::config::paths::home_dir()?;
        if crate::config::paths::is_real_user_home(&home)
            || !home.join(crate::config::paths::PRIVATE_HOME_MARKER).is_file()
        {
            return None;
        }
        Some(Self { app_data: Some(home.join(".local/share")), home })
    }

    /// The CC Switch data directory: the `app_config_dir_override` when it
    /// names an existing directory, else `<home>/.cc-switch` (Windows: also
    /// `%HOME%\.cc-switch` when the first has no database; not in tests).
    pub fn data_dir(&self) -> PathBuf {
        if let Some(dir) = self.override_dir() {
            return dir;
        }
        let dir = self.home.join(DIR_NAME);
        #[cfg(all(windows, not(test)))]
        if !dir.join(DB_FILE).is_file() {
            if let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) {
                let alternative = PathBuf::from(home).join(DIR_NAME);
                if alternative.join(DB_FILE).is_file() {
                    return alternative;
                }
            }
        }
        dir
    }

    fn override_dir(&self) -> Option<PathBuf> {
        let store = self.app_data.as_ref()?.join(APP_IDENTIFIER).join(APP_PATHS_FILE);
        let value = read_small_json(&store)?;
        let text = value.get(OVERRIDE_KEY)?.as_str()?.trim();
        if text.is_empty() {
            return None;
        }
        let path = if text == "~" {
            self.home.clone()
        } else if let Some(rest) = text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\")) {
            self.home.join(rest)
        } else {
            PathBuf::from(text)
        };
        (path.is_absolute() && path.is_dir()).then_some(path)
    }
}

fn read_small_json(path: &Path) -> Option<Value> {
    read_json(path, MAX_SMALL_JSON_BYTES).ok().flatten()
}

/// `Ok(None)` = absent; `Err` = present but unreadable / too large / not JSON.
fn read_json(path: &Path, max: u64) -> Result<Option<Value>, ()> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => metadata,
        _ => return Ok(None),
    };
    if metadata.len() > max {
        return Err(());
    }
    let bytes = std::fs::read(path).map_err(|_| ())?;
    serde_json::from_slice(&bytes).map(Some).map_err(|_| ())
}

/// Scan the data directory of [`Locations::current`].
pub fn scan_current() -> Scan {
    match Locations::current() {
        Some(locations) => scan_dir(&locations.data_dir()),
        None => Scan::default(),
    }
}

/// Scan one CC Switch data directory (database first, then `config.json`).
pub fn scan_dir(dir: &Path) -> Scan {
    let db = dir.join(DB_FILE);
    if db.is_file() {
        return scan_sqlite(dir, &db);
    }
    let json = dir.join(JSON_FILE);
    if json.is_file() {
        return scan_json(dir, &json);
    }
    Scan::default()
}

fn current_from_settings(dir: &Path) -> HashMap<App, String> {
    let mut current = HashMap::new();
    if let Some(settings) = read_small_json(&dir.join(SETTINGS_FILE)) {
        for (app, key) in [(App::Claude, "currentProviderClaude"), (App::Codex, "currentProviderCodex")] {
            if let Some(id) = settings.get(key).and_then(Value::as_str).filter(|id| !id.is_empty()) {
                current.insert(app, id.to_string());
            }
        }
    }
    current
}

fn unreadable(source: Source, what: &str) -> Scan {
    Scan {
        found: true,
        source: Some(source),
        error: Some(ScanError { code: "unreadable", message: format!("could not read CC Switch's {what}") }),
        providers: Vec::new(),
    }
}

fn scan_sqlite(dir: &Path, db: &Path) -> Scan {
    let mut source = Source { path: db.to_path_buf(), format: Format::Sqlite, schema_version: None };
    // Read-only: never creates the database, a journal or WAL files.
    let connection = match rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(connection) => connection,
        Err(_) => return unreadable(source, "database"),
    };
    let _ = connection.busy_timeout(BUSY_TIMEOUT);
    let _ = connection.pragma_update(None, "query_only", true);
    let version = match connection.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0)) {
        Ok(version) => version,
        Err(_) => return unreadable(source, "database"),
    };
    source.schema_version = Some(version);
    let rows = match read_rows(&connection) {
        Ok(Some(rows)) => rows,
        Ok(None) => return Scan {
            found: true,
            source: Some(source),
            error: Some(ScanError {
                code: if version > KNOWN_SCHEMA_VERSION { "unsupported_version" } else { "unsupported_format" },
                message: if version > KNOWN_SCHEMA_VERSION {
                    format!("CC Switch's provider format has changed (schema {version}; audited through {KNOWN_SCHEMA_VERSION}); update cutex before importing")
                } else {
                    "CC Switch's providers table is missing required fields (id, app_type, settings_config)".into()
                },
            }),
            providers: Vec::new(),
        },
        Err(_) => return unreadable(source, "providers table"),
    };
    let settings_current = current_from_settings(dir);
    let providers = rows
        .into_iter()
        .map(|row| {
            let current = match settings_current.get(&row.app) {
                Some(id) => *id == row.id,
                None => row.is_current,
            };
            parse_provider(row.id, row.app, row.name, &row.settings, row.category.as_deref(), &row.meta, current)
        })
        .collect();
    Scan { found: true, source: Some(source), error: None, providers }
}

struct Row {
    id: String,
    app: App,
    name: String,
    settings: Value,
    category: Option<String>,
    meta: Value,
    is_current: bool,
}

/// None means the provider layout is incompatible; SQL/IO errors remain
/// distinguishable so a busy/corrupt store is not reported as a newer format.
fn read_rows(connection: &rusqlite::Connection) -> rusqlite::Result<Option<Vec<Row>>> {
    // Columns differ across schema versions: select what exists.
    let mut columns = Vec::new();
    {
        let mut statement = connection.prepare("PRAGMA table_info(providers)")?;
        let names = statement.query_map([], |row| row.get::<_, String>(1))?;
        for name in names {
            columns.push(name?);
        }
    }
    let has = |name: &str| columns.iter().any(|column| column.eq_ignore_ascii_case(name));
    if !(has("id") && has("app_type") && has("settings_config")) {
        return Ok(None);
    }
    let column = |name: &str| if has(name) { name.to_string() } else { format!("NULL AS {name}") };
    let mut order = vec!["app_type".to_string()];
    if has("sort_index") {
        order.push("sort_index IS NULL, sort_index".into());
    }
    if has("created_at") {
        order.push("created_at".into());
    }
    order.push("id".into());
    let sql = format!(
        "SELECT id, app_type, {}, settings_config, {}, {}, {} FROM providers \
         WHERE app_type IN ('claude', 'codex') ORDER BY {} LIMIT {MAX_PROVIDERS}",
        column("name"),
        column("category"),
        column("meta"),
        column("is_current"),
        order.join(", "),
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map([], |row| {
        let text = |index: usize| -> rusqlite::Result<Option<String>> {
            Ok(match row.get_ref(index)? {
                rusqlite::types::ValueRef::Text(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
                rusqlite::types::ValueRef::Integer(value) => Some(value.to_string()),
                _ => None,
            })
        };
        let is_current = match row.get_ref(6)? {
            rusqlite::types::ValueRef::Integer(value) => value != 0,
            rusqlite::types::ValueRef::Text(bytes) => matches!(bytes, b"1" | b"true"),
            _ => false,
        };
        Ok((text(0)?, text(1)?, text(2)?, text(3)?, text(4)?, text(5)?, is_current))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, app, name, settings, category, meta, is_current) = row?;
        let (Some(id), Some(app)) = (id, app.as_deref().and_then(App::parse)) else {
            continue;
        };
        let parse = |text: Option<String>| text.and_then(|text| serde_json::from_str(&text).ok()).unwrap_or(Value::Null);
        out.push(Row {
            name: name.unwrap_or_else(|| id.clone()),
            id,
            app,
            settings: parse(settings),
            category,
            meta: parse(meta),
            is_current,
        });
    }
    Ok(Some(out))
}

fn scan_json(dir: &Path, path: &Path) -> Scan {
    let mut source = Source { path: path.to_path_buf(), format: Format::Json, schema_version: None };
    let value = match read_json(path, MAX_JSON_BYTES) {
        Ok(Some(value)) => value,
        _ => return unreadable(source, "config.json"),
    };
    let version = value.get("version").and_then(Value::as_i64);
    source.schema_version = version;
    if version != Some(2) {
        return Scan {
            found: true,
            source: Some(source),
            error: Some(ScanError {
                code: "unsupported_format",
                message: "this CC Switch config.json format is not supported (expected version 2)".into(),
            }),
            providers: Vec::new(),
        };
    }
    let settings_current = current_from_settings(dir);
    let mut providers = Vec::new();
    for app in [App::Claude, App::Codex] {
        let Some(section) = value.get(app.as_str()) else { continue };
        let current_id = settings_current
            .get(&app)
            .cloned()
            .or_else(|| section.get("current").and_then(Value::as_str).map(str::to_string));
        let Some(entries) = section.get("providers").and_then(Value::as_object) else { continue };
        let mut rows: Vec<(&String, &Value)> = entries.iter().collect();
        rows.sort_by_key(|(id, provider)| {
            (provider.get("sortIndex").and_then(Value::as_i64).unwrap_or(i64::MAX), (*id).clone())
        });
        for (key, provider) in rows.into_iter().take(MAX_PROVIDERS) {
            let id = provider.get("id").and_then(Value::as_str).unwrap_or(key).to_string();
            let name = provider.get("name").and_then(Value::as_str).unwrap_or(&id).to_string();
            let settings = provider
                .get("settingsConfig")
                .or_else(|| provider.get("settings_config"))
                .cloned()
                .unwrap_or(Value::Null);
            let category = provider.get("category").and_then(Value::as_str);
            let meta = provider.get("meta").cloned().unwrap_or(Value::Null);
            let current = current_id.as_deref() == Some(id.as_str());
            providers.push(parse_provider(id, app, name, &settings, category, &meta, current));
        }
    }
    Scan { found: true, source: Some(source), error: None, providers }
}

fn non_empty(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// A base URL for display: scheme, host, port and path only. `Err` when the
/// URL is unusable or carries credentials / a query (never shown).
fn display_url(text: &str) -> Result<String, &'static str> {
    let Ok(mut url) = url::Url::parse(text) else {
        return Err("its base URL is not a valid URL");
    };
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("its base URL is not an http(s) URL");
    }
    let hidden = !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some();
    if hidden {
        let _ = url.set_username("");
        let _ = url.set_password(None);
        url.set_query(None);
        url.set_fragment(None);
        return Err("its base URL carries credentials, a query or a fragment");
    }
    let shown = url.to_string();
    // `Url` adds a trailing `/` to a bare host; keep the owner's spelling.
    Ok(if text.trim().ends_with('/') { shown } else { shown.trim_end_matches('/').to_string() })
}

fn parse_provider(
    id: String,
    app: App,
    name: String,
    settings: &Value,
    category: Option<&str>,
    meta: &Value,
    current: bool,
) -> Provider {
    let mut provider = Provider {
        id,
        app,
        name,
        base_url: None,
        model: None,
        current,
        key: None,
        unsupported: None,
        claude_key_var: ClaudeKeyVar::AuthToken,
        claude_env: BTreeMap::new(),
    };
    let mut raw_url = None;
    match app {
        App::Claude => {
            let env = settings.get("env");
            let get = |key: &str| non_empty(env.and_then(|env| env.get(key)));
            raw_url = get("ANTHROPIC_BASE_URL");
            provider.model = get("ANTHROPIC_MODEL");
            let preferred = match meta.get("apiKeyField").and_then(Value::as_str) {
                Some("ANTHROPIC_API_KEY") => ClaudeKeyVar::ApiKey,
                _ => ClaudeKeyVar::AuthToken,
            };
            let order = match preferred {
                ClaudeKeyVar::AuthToken => [(ClaudeKeyVar::AuthToken, CLAUDE_KEY_VARS[0]), (ClaudeKeyVar::ApiKey, CLAUDE_KEY_VARS[1])],
                ClaudeKeyVar::ApiKey => [(ClaudeKeyVar::ApiKey, CLAUDE_KEY_VARS[1]), (ClaudeKeyVar::AuthToken, CLAUDE_KEY_VARS[0])],
            };
            provider.claude_key_var = preferred;
            for (var, name) in order {
                if let Some(key) = get(name) {
                    provider.claude_key_var = var;
                    provider.key = Some(Secret(key));
                    break;
                }
            }
            for allowed in crate::runtime::claude_accounts::API_ENV_ALLOWED {
                let value = env.and_then(|env| env.get(*allowed)).and_then(|value| match value {
                    Value::String(text) => Some(text.trim().to_string()),
                    Value::Number(number) => Some(number.to_string()),
                    Value::Bool(flag) => Some(if *flag { "1" } else { "0" }.to_string()),
                    _ => None,
                });
                if let Some(value) = value.filter(|value| {
                    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
                }) {
                    provider.claude_env.insert((*allowed).to_string(), value);
                }
            }
            if let Some(format) = meta.get("apiFormat").and_then(Value::as_str) {
                if format != "anthropic" {
                    provider.unsupported =
                        Some(format!("it needs CC Switch's local proxy to convert the API format ({format})"));
                }
            }
        }
        App::Codex => {
            provider.key = non_empty(settings.get("auth").and_then(|auth| auth.get("OPENAI_API_KEY"))).map(Secret);
            let config = settings.get("config").and_then(Value::as_str).unwrap_or_default();
            if !config.trim().is_empty() {
                match config.parse::<toml::Table>() {
                    Ok(root) => {
                        let section = root
                            .get("model_provider")
                            .and_then(toml::Value::as_str)
                            .and_then(|id| root.get("model_providers")?.get(id)?.as_table().cloned());
                        let text = |table: Option<&toml::Table>, key: &str| {
                            table?.get(key)?.as_str().map(str::trim).filter(|text| !text.is_empty()).map(str::to_string)
                        };
                        raw_url = text(section.as_ref(), "base_url").or_else(|| text(Some(&root), "base_url"));
                        provider.model = text(Some(&root), "model");
                        if let Some(wire) = text(section.as_ref(), "wire_api") {
                            if wire != "responses" {
                                provider.unsupported = Some(format!(
                                    "its wire_api \"{wire}\" is not supported (cutex Codex profiles use the Responses API)"
                                ));
                            }
                        }
                    }
                    // The parse error can quote config text: never shown.
                    Err(_) => provider.unsupported = Some("its config.toml could not be parsed".into()),
                }
            }
        }
    }
    if let Some(raw) = &raw_url {
        match display_url(raw) {
            Ok(url) => provider.base_url = Some(url),
            Err(reason) => provider.unsupported = provider.unsupported.take().or(Some(reason.into())),
        }
    }
    let provider_type = meta.get("providerType").and_then(Value::as_str);
    if let Some(kind) = provider_type.filter(|kind| OAUTH_PROVIDER_TYPES.contains(kind)) {
        provider.unsupported = Some(format!("it signs in with OAuth ({kind}), which cutex cannot import"));
    } else if category == Some("official") || (raw_url.is_none() && provider.key.is_none()) {
        provider.unsupported = Some(match app {
            App::Claude => "it is CC Switch's official Claude login entry (use a cutex Claude profile)".into(),
            App::Codex => "it is CC Switch's official OpenAI login entry (use a cutex ChatGPT profile)".into(),
        });
    }
    provider
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) const CLAUDE_KEY: &str = "sk-ccs-fixture-claude-0123456789";
    pub(crate) const CODEX_KEY: &str = "sk-ccs-fixture-codex-9876543210";

    thread_local! {
        pub(crate) static LOCATIONS_OVERRIDE: std::cell::RefCell<Option<Locations>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Makes [`Locations::current`] return `locations` on this thread until
    /// dropped. Refuses the real user profile.
    pub(crate) struct LocationsOverride(());

    impl LocationsOverride {
        pub(crate) fn new(locations: Locations) -> Self {
            assert!(
                !crate::config::paths::is_real_user_home(&locations.home),
                "CC Switch test locations must not be the real user home"
            );
            LOCATIONS_OVERRIDE.with(|cell| *cell.borrow_mut() = Some(locations));
            Self(())
        }
    }

    impl Drop for LocationsOverride {
        fn drop(&mut self) {
            LOCATIONS_OVERRIDE.with(|cell| *cell.borrow_mut() = None);
        }
    }

    /// A CC Switch v4-like database in `dir` (claude + codex + official + OAuth
    /// rows) and a settings.json naming the current Codex provider.
    pub(crate) fn write_fixture_db(dir: &Path, user_version: i64) {
        std::fs::create_dir_all(dir).unwrap();
        let connection = rusqlite::Connection::open(dir.join(DB_FILE)).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE providers (id TEXT NOT NULL, app_type TEXT NOT NULL, name TEXT NOT NULL,
                   settings_config TEXT NOT NULL, website_url TEXT, category TEXT, created_at INTEGER,
                   sort_index INTEGER, notes TEXT, icon TEXT, icon_color TEXT, meta TEXT NOT NULL DEFAULT '{}',
                   is_current BOOLEAN NOT NULL DEFAULT 0, in_failover_queue BOOLEAN NOT NULL DEFAULT 0,
                   PRIMARY KEY (id, app_type));
                 CREATE TABLE mcp_servers (id TEXT PRIMARY KEY, name TEXT NOT NULL,
                   server_config TEXT NOT NULL, enabled_pi BOOLEAN NOT NULL DEFAULT 0);
                 CREATE TABLE skills (id TEXT PRIMARY KEY, name TEXT NOT NULL, directory TEXT NOT NULL);",
            )
            .unwrap();
        // The relevant upstream 20 -> 23 migrations. These unrelated tables
        // deliberately evolve while provider records keep their known layout.
        for (version, table, flag) in [
            (21, "mcp_servers", "enabled_agy"), (21, "skills", "enabled_agy"),
            (22, "mcp_servers", "enabled_kimi"), (22, "skills", "enabled_kimi"),
            (23, "mcp_servers", "enabled_omp"),
        ] {
            if user_version >= version {
                connection.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {flag} BOOLEAN NOT NULL DEFAULT 0")).unwrap();
            }
        }
        connection.pragma_update(None, "user_version", user_version).unwrap();
        let insert = |id: &str, app: &str, name: &str, settings: Value, category: Option<&str>, meta: Value, current: bool, sort: i64| {
            connection
                .execute(
                    "INSERT INTO providers (id, app_type, name, settings_config, category, created_at, sort_index, meta, is_current)
                     VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7, ?8)",
                    rusqlite::params![id, app, name, settings.to_string(), category, sort, meta.to_string(), current],
                )
                .unwrap();
        };
        insert("claude-official", "claude", "Claude Official", json!({ "env": {} }), Some("official"), json!({}), false, 0);
        insert(
            "kimi",
            "claude",
            "Kimi K2",
            json!({ "env": {
                "ANTHROPIC_BASE_URL": "https://api.moonshot.cn/anthropic",
                "ANTHROPIC_API_KEY": CLAUDE_KEY,
                "ANTHROPIC_MODEL": "kimi-k2-turbo-preview",
                "ANTHROPIC_DEFAULT_HAIKU_MODEL": "kimi-k2-turbo-preview",
                "SOME_OTHER": "x",
            }}),
            Some("cn_official"),
            json!({ "apiKeyField": "ANTHROPIC_API_KEY" }),
            true,
            1,
        );
        insert(
            "copilot",
            "claude",
            "GitHub Copilot",
            json!({ "env": { "ANTHROPIC_BASE_URL": "https://api.githubcopilot.com" } }),
            Some("third_party"),
            json!({ "providerType": "github_copilot" }),
            false,
            2,
        );
        insert("codex-official", "codex", "OpenAI Official", json!({ "auth": {}, "config": "" }), Some("official"), json!({}), true, 0);
        insert(
            "relay",
            "codex",
            "My Relay",
            json!({
                "auth": { "OPENAI_API_KEY": CODEX_KEY },
                "config": "model_provider = \"relay\"\nmodel = \"gpt-5.1-codex\"\n\n[model_providers.relay]\nname = \"relay\"\nbase_url = \"https://relay.example.com/v1\"\nwire_api = \"responses\"\n",
            }),
            Some("third_party"),
            json!({}),
            false,
            1,
        );
        insert(
            "chat-relay",
            "codex",
            "Chat Relay",
            json!({
                "auth": { "OPENAI_API_KEY": CODEX_KEY },
                "config": "model_provider = \"c\"\nmodel = \"m\"\n[model_providers.c]\nbase_url = \"https://chat.example.com/v1\"\nwire_api = \"chat\"\n",
            }),
            Some("third_party"),
            json!({}),
            false,
            2,
        );
        insert("gemini-x", "gemini", "Gemini", json!({}), None, json!({}), false, 0);
        std::fs::write(dir.join(SETTINGS_FILE), json!({ "currentProviderCodex": "relay" }).to_string()).unwrap();
    }

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cutex-ccs-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sqlite_listing_current_and_support() {
        let root = temp_dir();
        let dir = root.join(DIR_NAME);
        write_fixture_db(&dir, 20);
        let before: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|entry| entry.unwrap().file_name()).collect();
        let scan = scan_dir(&dir);
        assert!(scan.found && scan.error.is_none(), "{scan:?}");
        assert_eq!(scan.source.as_ref().unwrap().format, Format::Sqlite);
        assert_eq!(scan.source.as_ref().unwrap().schema_version, Some(20));
        assert_eq!(scan.providers.len(), 6, "gemini rows are skipped");
        let kimi = scan.find("kimi", App::Claude).unwrap();
        assert_eq!(kimi.base_url.as_deref(), Some("https://api.moonshot.cn/anthropic"));
        assert_eq!(kimi.model.as_deref(), Some("kimi-k2-turbo-preview"));
        assert_eq!(kimi.claude_key_var, ClaudeKeyVar::ApiKey);
        assert_eq!(kimi.key.as_ref().unwrap().expose(), CLAUDE_KEY);
        assert!(kimi.current && kimi.unsupported.is_none());
        assert_eq!(kimi.claude_env.keys().collect::<Vec<_>>(), ["ANTHROPIC_DEFAULT_HAIKU_MODEL"]);
        assert!(!format!("{kimi:?}").contains(CLAUDE_KEY));
        let relay = scan.find("relay", App::Codex).unwrap();
        assert_eq!(relay.base_url.as_deref(), Some("https://relay.example.com/v1"));
        assert_eq!(relay.model.as_deref(), Some("gpt-5.1-codex"));
        assert!(relay.current, "settings.json wins over is_current");
        assert!(!scan.find("codex-official", App::Codex).unwrap().current);
        assert!(scan.find("codex-official", App::Codex).unwrap().unsupported.is_some());
        assert!(scan.find("claude-official", App::Claude).unwrap().unsupported.is_some());
        assert!(scan.find("copilot", App::Claude).unwrap().unsupported.as_deref().unwrap().contains("OAuth"));
        assert!(scan.find("chat-relay", App::Codex).unwrap().unsupported.as_deref().unwrap().contains("wire_api"));
        // Read-only: no journal / WAL / shm files appeared.
        let after: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|entry| entry.unwrap().file_name()).collect();
        assert_eq!(before, after);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn schema_21_to_23_and_future_additions_are_read_without_changes() {
        for version in [20, 21, 22, 23, 24, 999] {
            let root = temp_dir();
            write_fixture_db(&root, version);
            if version > KNOWN_SCHEMA_VERSION {
                let connection = rusqlite::Connection::open(root.join(DB_FILE)).unwrap();
                connection.execute_batch("ALTER TABLE providers ADD COLUMN future_option TEXT;
                    CREATE TABLE future_state (id TEXT PRIMARY KEY, payload TEXT);").unwrap();
            }
            let before = std::fs::read(root.join(DB_FILE)).unwrap();
            let mut files = std::fs::read_dir(&root).unwrap().map(|entry| entry.unwrap().file_name()).collect::<Vec<_>>();
            files.sort();
            let scan = scan_dir(&root);
            assert!(scan.found && scan.error.is_none(), "schema {version}: {scan:?}");
            assert_eq!(scan.source.as_ref().unwrap().schema_version, Some(version));
            assert_eq!(scan.providers.len(), 6);
            let claude = scan.find("kimi", App::Claude).unwrap();
            let codex = scan.find("relay", App::Codex).unwrap();
            assert_eq!(claude.key.as_ref().unwrap().expose(), CLAUDE_KEY);
            assert_eq!(codex.key.as_ref().unwrap().expose(), CODEX_KEY);
            assert_eq!(codex.model.as_deref(), Some("gpt-5.1-codex"));
            assert!(claude.current && codex.current);
            assert!(claude.unsupported.is_none() && codex.unsupported.is_none());
            let debug = format!("{scan:?}");
            assert!(!debug.contains(CLAUDE_KEY) && !debug.contains(CODEX_KEY));
            assert_eq!(before, std::fs::read(root.join(DB_FILE)).unwrap());
            let mut after = std::fs::read_dir(&root).unwrap().map(|entry| entry.unwrap().file_name()).collect::<Vec<_>>();
            after.sort();
            assert_eq!(files, after, "schema {version}: read must not create sidecars");
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn missing_optional_provider_columns_do_not_block_future_schemas() {
        let root = temp_dir();
        let connection = rusqlite::Connection::open(root.join(DB_FILE)).unwrap();
        connection.execute_batch("CREATE TABLE providers (ID TEXT, APP_TYPE TEXT, SETTINGS_CONFIG TEXT);
            PRAGMA user_version=999;").unwrap();
        let settings = json!({"auth":{"OPENAI_API_KEY":CODEX_KEY},
            "config":"base_url = \"https://relay.example.com/v1\"\nmodel = \"fixture-model\"\n"});
        connection.execute("INSERT INTO providers VALUES (?1, ?2, ?3)",
            rusqlite::params!["minimal", "codex", settings.to_string()]).unwrap();
        drop(connection);
        let scan = scan_dir(&root);
        assert!(scan.error.is_none(), "{scan:?}");
        let provider = scan.find("minimal", App::Codex).unwrap();
        assert_eq!(provider.name, "minimal");
        assert_eq!(provider.key.as_ref().unwrap().expose(), CODEX_KEY);
        assert_eq!(provider.model.as_deref(), Some("fixture-model"));
        assert!(!provider.current && provider.unsupported.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn incompatible_provider_layouts_have_stable_secret_free_codes() {
        for version in [23, 24, 999] {
            for missing in ["id", "app_type", "settings_config", "providers"] {
                let root = temp_dir();
                write_fixture_db(&root, version);
                let connection = rusqlite::Connection::open(root.join(DB_FILE)).unwrap();
                connection.execute_batch("ALTER TABLE providers RENAME TO future_providers").unwrap();
                if missing != "providers" {
                    let fields = ["id TEXT", "app_type TEXT", "settings_config TEXT"].into_iter()
                        .filter(|field| !field.starts_with(missing)).collect::<Vec<_>>().join(", ");
                    connection.execute_batch(&format!("CREATE TABLE providers ({fields})")).unwrap();
                }
                drop(connection);
                let before = std::fs::read(root.join(DB_FILE)).unwrap();
                let scan = scan_dir(&root);
                assert!(scan.found && scan.providers.is_empty());
                let error = scan.error.unwrap();
                assert_eq!(error.code, if version > KNOWN_SCHEMA_VERSION { "unsupported_version" } else { "unsupported_format" });
                assert!(!error.message.contains(CLAUDE_KEY) && !error.message.contains(CODEX_KEY));
                assert_eq!(scan.source.unwrap().schema_version, Some(version));
                assert_eq!(before, std::fs::read(root.join(DB_FILE)).unwrap());
                std::fs::remove_dir_all(root).unwrap();
            }
        }
    }

    #[test]
    fn corrupt_sqlite_is_unreadable_rather_than_incompatible() {
        let root = temp_dir();
        std::fs::write(root.join(DB_FILE), b"synthetic invalid sqlite").unwrap();
        let scan = scan_dir(&root);
        assert!(scan.found && scan.providers.is_empty());
        assert_eq!(scan.error.unwrap().code, "unreadable");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_dir_is_not_found() {
        let root = temp_dir();
        let scan = scan_dir(&root.join(DIR_NAME));
        assert!(!scan.found && scan.source.is_none() && scan.error.is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn json_v2_config_snake_and_camel_case() {
        let root = temp_dir();
        let config = json!({
            "version": 2,
            "claude": {
                "current": "glm",
                "providers": {
                    "glm": { "id": "glm", "name": "GLM", "settingsConfig": { "env": {
                        "ANTHROPIC_BASE_URL": "https://open.bigmodel.cn/api/anthropic",
                        "ANTHROPIC_AUTH_TOKEN": CLAUDE_KEY,
                    }}},
                    "default": { "id": "default", "name": "Claude Official", "category": "official", "settingsConfig": { "env": {} } },
                }
            },
            "codex": {
                "current": "x",
                "providers": {
                    "x": { "id": "x", "name": "X", "settings_config": {
                        "auth": { "OPENAI_API_KEY": CODEX_KEY },
                        "config": "base_url = \"https://x.example.com/v1\"\nmodel = \"m1\"\n",
                    }},
                    "oauth": { "id": "oauth", "name": "Codex OAuth", "settingsConfig": { "auth": {}, "config": "" },
                               "meta": { "providerType": "codex_oauth" } },
                }
            }
        });
        std::fs::write(root.join(JSON_FILE), config.to_string()).unwrap();
        let scan = scan_dir(&root);
        assert!(scan.found && scan.error.is_none(), "{scan:?}");
        assert_eq!(scan.source.as_ref().unwrap().format, Format::Json);
        let glm = scan.find("glm", App::Claude).unwrap();
        assert!(glm.current && glm.unsupported.is_none());
        assert_eq!(glm.claude_key_var, ClaudeKeyVar::AuthToken);
        assert!(scan.find("default", App::Claude).unwrap().unsupported.is_some());
        let x = scan.find("x", App::Codex).unwrap();
        assert_eq!((x.base_url.as_deref(), x.model.as_deref()), (Some("https://x.example.com/v1"), Some("m1")));
        assert!(x.current && x.key.is_some());
        assert!(scan.find("oauth", App::Codex).unwrap().unsupported.as_deref().unwrap().contains("OAuth"));

        std::fs::write(root.join(JSON_FILE), json!({ "providers": {}, "current": "" }).to_string()).unwrap();
        assert_eq!(scan_dir(&root).error.unwrap().code, "unsupported_format");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn data_dir_override_and_url_hygiene() {
        let root = temp_dir();
        let moved = root.join("elsewhere");
        std::fs::create_dir_all(&moved).unwrap();
        let app_data = root.join("data");
        std::fs::create_dir_all(app_data.join(APP_IDENTIFIER)).unwrap();
        let locations = Locations { home: root.clone(), app_data: Some(app_data.clone()) };
        assert_eq!(locations.data_dir(), root.join(DIR_NAME));
        std::fs::write(app_data.join(APP_IDENTIFIER).join(APP_PATHS_FILE), json!({ OVERRIDE_KEY: "~/elsewhere" }).to_string()).unwrap();
        assert_eq!(locations.data_dir(), moved);
        std::fs::write(app_data.join(APP_IDENTIFIER).join(APP_PATHS_FILE), json!({ OVERRIDE_KEY: root.join("missing") }).to_string()).unwrap();
        assert_eq!(locations.data_dir(), root.join(DIR_NAME), "a missing override falls back");

        assert_eq!(display_url("https://a.example.com/v1"), Ok("https://a.example.com/v1".into()));
        assert_eq!(display_url("https://a.example.com"), Ok("https://a.example.com".into()));
        assert!(display_url("https://u:secret@a.example.com/v1").is_err());
        assert!(display_url("https://a.example.com/v1?key=secret").is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}
