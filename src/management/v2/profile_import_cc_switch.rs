//! Import Profiles from CC Switch (capability `profile-import-cc-switch`;
//! owner / root only, like `POST /v2/profiles`). The reader is
//! [`crate::profiles::cc_switch`]; docs/management/profile-import-cc-switch.md.
//!
//! ```text
//! GET  /v2/profile-imports/cc-switch
//!   → 200 {contractVersion, found, source: {path, format, schemaVersion} | null,
//!          error: {code, message} | null, providers: [Provider]}
//! POST /v2/profile-imports/cc-switch {items: [{id, app, name?}]}   (1-50 items)
//!   → 200 {contractVersion, created: [{id, app, profile}], failed: [{id, app, code, message}]}
//! ```
//!
//! Provider: `{id, app: claude|codex, name, baseUrl, model, current, hasKey,
//! supported, reason, existingProfile}`. Keys never leave this module: not in
//! listings, results, errors or logs.
//!
//! Each import item is created through [`super::profiles_api::create_from_body`]
//! (the `POST /v2/profiles` path): a Codex row becomes an `apiKey` profile
//! (`baseUrl`, `apiKey`, `model`); a Claude row becomes a Claude API account
//! (`crate::runtime::claude_accounts`, base URL, key and the key variable,
//! model alias settings) plus a `claude` profile on it with the model.

use std::collections::HashSet;
use std::net::TcpStream;

use serde_json::{json, Map, Value};

use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::profiles::cc_switch::{self, App, ClaudeKeyVar, Provider, Scan};
use crate::profiles::lifecycle;
use crate::profiles::model::{AccountsStore, CliKind};
use crate::runtime::claude_accounts;

pub const PATH: &str = "/v2/profile-imports/cc-switch";
pub const MAX_ITEMS: usize = 50;
const MAX_ID_CHARS: usize = 256;
const MAX_NAME_CHARS: usize = 64;
/// Codex rows without a base URL talk to OpenAI.
const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

pub fn is_path(path: &str) -> bool {
    path.split('?').next() == Some(PATH)
}

pub(super) fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    match request.method.as_str() {
        "GET" => write_json_response(stream, 200, "OK", &list_resource(&cc_switch::scan_current())),
        "POST" => match parse_items(&request.body) {
            Ok(items) => write_json_response(stream, 200, "OK", &import(&cc_switch::scan_current(), &items)),
            Err(message) => super::server::write_v2_error(
                stream,
                400,
                "Bad Request",
                "invalid_request",
                &message,
                false,
                json!({}),
            ),
        },
        _ => super::server::write_v2_error(
            stream,
            405,
            "Method Not Allowed",
            "method_not_allowed",
            "use GET or POST for /v2/profile-imports/cc-switch",
            false,
            json!({}),
        ),
    }
}

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

fn normalize_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_ascii_lowercase()
}

/// (app, base URL, model) of every cutex profile, for `existingProfile`.
fn profile_signatures(store: &AccountsStore) -> Vec<(App, String, Option<String>, String)> {
    let registry = claude_accounts::load().unwrap_or_default();
    store
        .accounts
        .iter()
        .filter_map(|account| {
            let (app, base, model) = match account.cli_kind {
                CliKind::Codex => (
                    App::Codex,
                    crate::profiles::inspect::account_model_api_base(account)?,
                    crate::profiles::settings_api::codex_snapshot(account).model,
                ),
                CliKind::Claude => {
                    let id = account.claude_account.as_deref()?;
                    let base = registry.find(id)?.api.as_ref()?.base_url.clone()?;
                    (App::Claude, base, Some(account.options.claude.model.clone()).filter(|model| !model.is_empty()))
                }
            };
            Some((app, normalize_url(&base), model, account.name.clone()))
        })
        .collect()
}

fn existing_profile(signatures: &[(App, String, Option<String>, String)], provider: &Provider) -> Option<String> {
    let base = normalize_url(provider.base_url.as_deref()?);
    signatures
        .iter()
        .find(|(app, url, model, _)| *app == provider.app && *url == base && *model == provider.model)
        .map(|(_, _, _, name)| name.clone())
}

pub fn list_resource(scan: &Scan) -> Value {
    let signatures = crate::profiles::settings_api::load_store()
        .map(|store| profile_signatures(&store))
        .unwrap_or_default();
    json!({
        "contractVersion": 2,
        "found": scan.found,
        "source": scan.source.as_ref().map(|source| json!({
            "path": source.path.display().to_string(),
            "format": source.format.as_str(),
            "schemaVersion": source.schema_version,
        })),
        "error": scan.error.as_ref().map(|error| json!({ "code": error.code, "message": error.message })),
        "providers": scan.providers.iter().map(|provider| json!({
            "id": provider.id,
            "app": provider.app.as_str(),
            "name": provider.name,
            "baseUrl": provider.base_url,
            "model": provider.model,
            "current": provider.current,
            "hasKey": provider.key.is_some(),
            "supported": provider.unsupported.is_none(),
            "reason": provider.unsupported,
            "existingProfile": existing_profile(&signatures, provider),
        })).collect::<Vec<_>>(),
    })
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Item {
    id: String,
    app: App,
    name: Option<String>,
}

pub(super) fn parse_items(body: &[u8]) -> Result<Vec<Item>, String> {
    let value: Value = serde_json::from_slice(body).map_err(|_| "JSON object body {items} required".to_string())?;
    let Value::Object(body) = value else {
        return Err("JSON object body {items} required".into());
    };
    if let Some(unknown) = body.keys().find(|key| key.as_str() != "items") {
        return Err(format!("unsupported field: {unknown}"));
    }
    let Some(Value::Array(entries)) = body.get("items") else {
        return Err("items: required array".into());
    };
    if entries.is_empty() || entries.len() > MAX_ITEMS {
        return Err(format!("items: 1-{MAX_ITEMS} entries"));
    }
    let mut seen = HashSet::new();
    let mut items = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let item = parse_item(entry).map_err(|message| format!("items[{index}]: {message}"))?;
        if !seen.insert((item.id.clone(), item.app)) {
            return Err(format!("items[{index}]: duplicate {} provider {}", item.app.as_str(), item.id));
        }
        items.push(item);
    }
    Ok(items)
}

fn parse_item(entry: &Value) -> Result<Item, String> {
    let Value::Object(entry) = entry else {
        return Err("must be an object {id, app, name?}".into());
    };
    if let Some(unknown) = entry.keys().find(|key| !["id", "app", "name"].contains(&key.as_str())) {
        return Err(format!("unsupported field: {unknown}"));
    }
    let id = match entry.get("id") {
        Some(Value::String(id))
            if !id.is_empty() && id.chars().count() <= MAX_ID_CHARS && !id.chars().any(char::is_control) =>
        {
            id.clone()
        }
        _ => return Err(format!("id: required string of 1-{MAX_ID_CHARS} characters")),
    };
    let app = entry
        .get("app")
        .and_then(Value::as_str)
        .and_then(App::parse)
        .ok_or_else(|| "app: must be claude or codex".to_string())?;
    let name = match entry.get("name") {
        None | Some(Value::Null) => None,
        Some(Value::String(name)) => Some(lifecycle::validate_profile_name(name).map_err(|message| format!("name: {message}"))?),
        Some(_) => return Err("name: must be a string".into()),
    };
    Ok(Item { id, app, name })
}

struct Failure {
    code: &'static str,
    message: String,
}

impl Failure {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

pub(super) fn import(scan: &Scan, items: &[Item]) -> Value {
    let mut created = Vec::new();
    let mut failed = Vec::new();
    for item in items {
        match import_one(scan, item) {
            Ok(profile) => created.push(json!({ "id": item.id, "app": item.app.as_str(), "profile": profile })),
            Err(failure) => failed.push(json!({
                "id": item.id,
                "app": item.app.as_str(),
                "code": failure.code,
                "message": failure.message,
            })),
        }
    }
    json!({ "contractVersion": 2, "created": created, "failed": failed })
}

/// A profile / account name from a CC Switch name: no `/`, `\` or control
/// characters, at most 64 characters.
fn clean_name(name: &str, app: App) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c == '/' || c == '\\' { '-' } else if c.is_control() { ' ' } else { c })
        .collect();
    let cleaned: String = cleaned.trim().chars().take(MAX_NAME_CHARS).collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() {
        format!("CC Switch {}", app.as_str())
    } else {
        cleaned
    }
}

/// `base`, else `base (2)`, `base (3)`… (within 64 characters).
fn unique(base: &str, taken: impl Fn(&str) -> bool) -> Option<String> {
    if !taken(base) {
        return Some(base.to_string());
    }
    (2..1000).find_map(|n| {
        let suffix = format!(" ({n})");
        let keep = MAX_NAME_CHARS - suffix.chars().count();
        let candidate = format!("{}{suffix}", base.chars().take(keep).collect::<String>().trim_end());
        (!taken(&candidate)).then_some(candidate)
    })
}

fn name_taken(store: &AccountsStore, name: &str) -> bool {
    store.accounts.iter().any(|account| account.name == name || account.id == name)
}

fn profile_name(item: &Item, provider: &Provider) -> Result<String, Failure> {
    let store = crate::profiles::settings_api::load_store()
        .map_err(|_| Failure::new("store_unavailable", "the profile store could not be read"))?;
    match &item.name {
        Some(name) if name_taken(&store, name) => {
            Err(Failure::new("profile_exists", format!("a profile named {name} already exists")))
        }
        Some(name) => Ok(name.clone()),
        None => unique(&clean_name(&provider.name, provider.app), |name| name_taken(&store, name))
            .ok_or_else(|| Failure::new("profile_exists", "no free profile name for this provider")),
    }
}

/// Never let a key reach a message.
fn scrub(message: String, provider: &Provider) -> String {
    match &provider.key {
        Some(key) if message.contains(key.expose()) => "the provider's settings were refused".into(),
        _ => message,
    }
}

fn create(body: Value, provider: &Provider) -> Result<Value, Failure> {
    match super::profiles_api::create_from_body(body.to_string().as_bytes()) {
        Ok(created) => Ok(created["profile"].clone()),
        Err(failure) => {
            let code = match (failure.status, failure.code) {
                (_, "profile_name_conflict") => "profile_exists",
                (503, _) => "store_unavailable",
                _ => "invalid",
            };
            Err(Failure::new(code, scrub(failure.message, provider)))
        }
    }
}

fn import_one(scan: &Scan, item: &Item) -> Result<Value, Failure> {
    if let Some(error) = &scan.error {
        let code = if error.code == "unreadable" { "provider_gone" } else { "unsupported" };
        return Err(Failure::new(code, error.message.clone()));
    }
    let Some(provider) = scan.find(&item.id, item.app) else {
        return Err(Failure::new("provider_gone", "CC Switch no longer has this provider"));
    };
    if let Some(reason) = &provider.unsupported {
        return Err(Failure::new("unsupported", format!("this provider cannot be imported: {reason}")));
    }
    let Some(key) = &provider.key else {
        return Err(Failure::new("no_key", "this provider has no API key in CC Switch"));
    };
    let name = profile_name(item, provider)?;
    match provider.app {
        App::Codex => {
            let mut body = Map::new();
            body.insert("name".into(), json!(name));
            body.insert("kind".into(), json!("apiKey"));
            body.insert("baseUrl".into(), json!(provider.base_url.as_deref().unwrap_or(OPENAI_BASE_URL)));
            body.insert("apiKey".into(), json!(key.expose()));
            if let Some(model) = &provider.model {
                body.insert("model".into(), json!(model));
            }
            create(Value::Object(body), provider)
        }
        App::Claude => {
            let registry = claude_accounts::load()
                .map_err(|_| Failure::new("store_unavailable", "the Claude account registry could not be read"))?;
            let label = unique(&name, |label| {
                label.eq_ignore_ascii_case(claude_accounts::DEFAULT_ACCOUNT_ID)
                    || registry.accounts.iter().any(|account| account.label == label)
            })
            .ok_or_else(|| Failure::new("invalid", "no free Claude account label for this provider"))?;
            let account = claude_accounts::add(claude_accounts::NewClaudeAccount {
                label,
                kind: claude_accounts::ClaudeAccountKind::Api,
                api: Some(claude_accounts::ClaudeApiSettings {
                    base_url: provider.base_url.clone(),
                    key_env: match provider.claude_key_var {
                        ClaudeKeyVar::AuthToken => claude_accounts::ApiKeyEnv::AuthToken,
                        ClaudeKeyVar::ApiKey => claude_accounts::ApiKeyEnv::ApiKey,
                    },
                    env: provider.claude_env.clone(),
                }),
                api_key: Some(key.expose().to_string()),
            })
            .map_err(|error| Failure::new("invalid", scrub(format!("Claude account: {error:#}"), provider)))?;
            let mut body = Map::new();
            body.insert("name".into(), json!(name));
            body.insert("kind".into(), json!("claude"));
            body.insert("claudeAccount".into(), json!(account.id));
            if let Some(model) = &provider.model {
                body.insert("model".into(), json!(model));
            }
            let created = create(Value::Object(body), provider);
            if created.is_err() {
                if let Err(error) = claude_accounts::remove(&account.id, true) {
                    eprintln!("cc-switch import: could not remove Claude account {}: {error:?}", account.id);
                }
            }
            created
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::cc_switch::tests::{write_fixture_db, LocationsOverride, CLAUDE_KEY, CODEX_KEY};

    /// One request through `profiles_api::handle` (the dispatch the server uses).
    fn call(method: &str, body: &str) -> (u16, String) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            use std::io::Read;
            let mut stream = std::net::TcpStream::connect(address).unwrap();
            let mut bytes = Vec::new();
            stream.read_to_end(&mut bytes).unwrap();
            String::from_utf8_lossy(&bytes).into_owned()
        });
        let (mut stream, _) = listener.accept().unwrap();
        let request = SimpleHttpRequest {
            method: method.into(),
            path: PATH.into(),
            headers: Default::default(),
            body: body.as_bytes().to_vec(),
        };
        assert!(super::super::profiles_api::is_profiles_path(PATH));
        super::super::profiles_api::handle(&mut stream, &request, PATH).unwrap();
        drop(stream);
        let response = client.join().unwrap();
        let status = response[9..12].parse().unwrap();
        let body = response.split_once("\r\n\r\n").map(|(_, body)| body.to_string()).unwrap_or_default();
        (status, body)
    }

    fn json_of(body: &str) -> Value {
        serde_json::from_str(body).unwrap()
    }

    #[test]
    fn list_and_import_through_a_test_home() {
        let test_home = crate::profiles::lifecycle::tests::TestHome::new();
        // An explicit base dir: never the user's CC Switch data (the real
        // profile on Windows, whatever HOME says).
        let home = test_home.root().join("cc-switch-home");
        std::fs::create_dir_all(&home).unwrap();
        let _locations = LocationsOverride::new(cc_switch::Locations {
            home: home.clone(),
            app_data: Some(home.join("app-data")),
        });

        // No CC Switch: found = false, still 200.
        let (status, body) = call("GET", "");
        assert_eq!(status, 200, "{body}");
        let empty = json_of(&body);
        assert_eq!((empty["found"].as_bool(), &empty["source"], &empty["error"]), (Some(false), &Value::Null, &Value::Null));

        write_fixture_db(&home.join(cc_switch::DIR_NAME), 20);
        let (status, body) = call("GET", "");
        assert_eq!(status, 200, "{body}");
        assert!(!body.contains(CLAUDE_KEY) && !body.contains(CODEX_KEY), "listing carries no key: {body}");
        let list = json_of(&body);
        assert_eq!(list["found"], true);
        assert_eq!(list["source"]["format"], "sqlite");
        assert_eq!(list["source"]["schemaVersion"], 20);
        let row = |id: &str| list["providers"].as_array().unwrap().iter().find(|row| row["id"] == id).unwrap().clone();
        let kimi = row("kimi");
        assert_eq!(kimi["app"], "claude");
        assert_eq!((kimi["hasKey"].as_bool(), kimi["supported"].as_bool(), kimi["current"].as_bool()), (Some(true), Some(true), Some(true)));
        assert_eq!(kimi["existingProfile"], Value::Null);
        assert_eq!(row("relay")["current"], true);
        assert_eq!(row("codex-official")["supported"], false);
        assert!(row("copilot")["reason"].as_str().unwrap().contains("OAuth"));

        // Strict bodies.
        for bad in [
            r#"{}"#,
            r#"{"items":[]}"#,
            r#"{"items":[{"id":"kimi","app":"claude"}],"extra":1}"#,
            r#"{"items":[{"id":"kimi","app":"claude","apiKey":"x"}]}"#,
            r#"{"items":[{"id":"kimi","app":"gemini"}]}"#,
            r#"{"items":[{"id":"kimi","app":"claude"},{"id":"kimi","app":"claude"}]}"#,
        ] {
            let (status, body) = call("POST", bad);
            assert_eq!(status, 400, "{bad}: {body}");
            assert_eq!(json_of(&body)["error"]["code"], "invalid_request");
        }
        let too_many = json!({ "items": (0..51).map(|n| json!({ "id": n.to_string(), "app": "codex" })).collect::<Vec<_>>() });
        assert_eq!(call("POST", &too_many.to_string()).0, 400);

        let request = json!({ "items": [
            { "id": "kimi", "app": "claude" },
            { "id": "relay", "app": "codex", "name": "Relay" },
            { "id": "codex-official", "app": "codex" },
            { "id": "chat-relay", "app": "codex" },
            { "id": "gone", "app": "codex" },
        ]});
        let (status, body) = call("POST", &request.to_string());
        assert_eq!(status, 200, "{body}");
        assert!(!body.contains(CLAUDE_KEY) && !body.contains(CODEX_KEY), "import result carries no key: {body}");
        let result = json_of(&body);
        let created = result["created"].as_array().unwrap();
        assert_eq!(created.len(), 2, "{result}");
        let claude = &created[0]["profile"];
        assert_eq!((created[0]["id"].as_str(), created[0]["app"].as_str()), (Some("kimi"), Some("claude")));
        assert_eq!(claude["name"], "Kimi K2");
        assert_eq!(claude["cliKind"], "claude");
        let account_id = claude["claudeAccount"].as_str().or_else(|| claude["claudeAccount"]["id"].as_str()).unwrap().to_string();
        let registry = claude_accounts::load().unwrap();
        let account = registry.find(&account_id).unwrap();
        assert_eq!(account.kind, claude_accounts::ClaudeAccountKind::Api);
        let api = account.api.as_ref().unwrap();
        assert_eq!(api.base_url.as_deref(), Some("https://api.moonshot.cn/anthropic"));
        assert_eq!(api.key_env, claude_accounts::ApiKeyEnv::ApiKey);
        assert_eq!(api.env.get("ANTHROPIC_DEFAULT_HAIKU_MODEL").map(String::as_str), Some("kimi-k2-turbo-preview"));
        assert!(claude_accounts::api_key_present(&account_id));
        let codex = &created[1]["profile"];
        assert_eq!(codex["name"], "Relay");
        assert_eq!(codex["baseUrl"], "https://relay.example.com/v1");
        assert_eq!(codex["model"], "gpt-5.1-codex");
        assert_eq!(codex["authPresent"], true);
        let failed = result["failed"].as_array().unwrap();
        let code = |id: &str| failed.iter().find(|row| row["id"] == id).unwrap()["code"].as_str().unwrap().to_string();
        assert_eq!(code("codex-official"), "unsupported");
        assert_eq!(code("chat-relay"), "unsupported");
        assert_eq!(code("gone"), "provider_gone");

        // The listing now names the imported profiles.
        let list = json_of(&call("GET", "").1);
        let row = |id: &str| list["providers"].as_array().unwrap().iter().find(|row| row["id"] == id).unwrap().clone();
        assert_eq!(row("kimi")["existingProfile"], "Kimi K2");
        assert_eq!(row("relay")["existingProfile"], "Relay");

        // Again: an explicit name clashes, the default name is made unique.
        let (_, body) = call("POST", r#"{"items":[{"id":"relay","app":"codex","name":"Relay"},{"id":"kimi","app":"claude"}]}"#);
        let again = json_of(&body);
        assert_eq!(again["failed"][0]["code"], "profile_exists", "{again}");
        assert_eq!(again["created"][0]["profile"]["name"], "Kimi K2 (2)", "{again}");

        // A newer CC Switch: listed as an error, imports refused.
        let dir = home.join(cc_switch::DIR_NAME);
        std::fs::remove_dir_all(&dir).unwrap();
        write_fixture_db(&dir, 21);
        let list = json_of(&call("GET", "").1);
        assert_eq!(list["error"]["code"], "unsupported_version");
        assert_eq!(list["providers"], json!([]));
        let (_, body) = call("POST", r#"{"items":[{"id":"kimi","app":"claude"}]}"#);
        assert_eq!(json_of(&body)["failed"][0]["code"], "unsupported");
        assert_eq!(call("DELETE", "").0, 405);
    }

    #[test]
    fn names_are_cleaned_and_made_unique() {
        assert_eq!(clean_name(" a/b\\c ", App::Codex), "a-b-c");
        assert_eq!(clean_name("  ", App::Claude), "CC Switch claude");
        assert_eq!(clean_name(&"x".repeat(80), App::Codex).chars().count(), 64);
        let taken = ["A".to_string(), "A (2)".to_string()];
        assert_eq!(unique("A", |name| taken.iter().any(|t| t == name)).as_deref(), Some("A (3)"));
        let long = "y".repeat(64);
        let made = unique(&long, |name| name == long).unwrap();
        assert_eq!(made.chars().count(), 64);
        assert!(made.ends_with(" (2)"));
    }
}
