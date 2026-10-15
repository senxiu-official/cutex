//! Claude accounts over Management v2 (capability `claude-accounts`; root
//! credential incl. client / device tokens). See
//! [`crate::runtime::claude_accounts`] and cutex-desktop
//! docs/research/claude-multi-account.md.
//!
//! ```text
//! GET    /v2/claude/accounts                   → {contractVersion, accounts: Item[]}
//! POST   /v2/claude/accounts                   {label, kind?, baseUrl?, keyEnv?, env?, apiKey?} → 201 Item
//! GET    /v2/claude/accounts/{id}              → Item
//! PATCH  /v2/claude/accounts/{id}              {label?, baseUrl?|null, keyEnv?, env?, apiKey?} → Item
//! DELETE /v2/claude/accounts/{id}[?deleteHome=true] → {id, removed: true, homeDeleted, keptHome}
//! POST   /v2/claude/accounts/{id}/status       → Item (runs `claude auth status`; API: key presence)
//! POST   /v2/claude/accounts/{id}/logout       → Item (`claude auth logout`, subscription only)
//! ```
//!
//! `id` is `default` (the shared home, implicit, not removable, no settings)
//! or `ca-…`. `kind` is `subscription` (default) or `api`; `baseUrl`,
//! `keyEnv` (`ANTHROPIC_API_KEY` | `ANTHROPIC_AUTH_TOKEN`), `env` (allowed
//! non-secret Claude Code settings) and `apiKey` apply to API accounts only.
//! `apiKey` is write-only (items say `api.apiKeyPresent`). There is no login
//! over HTTP (owner decision 2): items of subscription accounts carry
//! `loginCommand {cutex, powershell, posix, claudeConfigDir}` for the owner's
//! terminal, then the client calls `…/status`.
//!
//! Errors: 400 `invalid_request`; 404 `claude_account_not_found`; 409
//! `claude_account_label_taken`, `claude_account_is_default`,
//! `claude_account_in_use` (details: profiles, sessions),
//! `claude_account_kind_mismatch`; 500 `claude_accounts_failed`.

use std::collections::BTreeMap;
use std::net::TcpStream;

use serde_json::{json, Map, Value};

use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::runtime::claude_accounts::{
    self, AccountPatch, ApiKeyEnv, Change, ClaudeAccountKind, ClaudeApiSettings, NewClaudeAccount, RemoveError,
    DEFAULT_ACCOUNT_ID,
};

pub const PATH: &str = "/v2/claude/accounts";

pub fn is_path(path: &str) -> bool {
    path == PATH || path.starts_with("/v2/claude/accounts/")
}

#[derive(Debug, PartialEq, Eq)]
enum Route<'a> {
    Collection,
    Item(&'a str),
    Status(&'a str),
    Logout(&'a str),
}

fn route(path: &str) -> Option<Route<'_>> {
    let path = path.split('?').next().unwrap_or(path);
    if path == PATH {
        return Some(Route::Collection);
    }
    let rest = path.strip_prefix("/v2/claude/accounts/")?;
    let (id, action) = match rest.split_once('/') {
        Some((id, action)) => (id, Some(action)),
        None => (rest, None),
    };
    if id != DEFAULT_ACCOUNT_ID && claude_accounts::validate_id(id).is_err() {
        return None;
    }
    match action {
        None => Some(Route::Item(id)),
        Some("status") => Some(Route::Status(id)),
        Some("logout") => Some(Route::Logout(id)),
        Some(_) => None,
    }
}

fn delete_home_requested(path: &str) -> Result<bool, String> {
    let Some((_, query)) = path.split_once('?') else {
        return Ok(false);
    };
    let mut delete_home = false;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        match pair.split_once('=').unwrap_or((pair, "")) {
            ("deleteHome", "true" | "1") => delete_home = true,
            ("deleteHome", "false" | "0") => delete_home = false,
            ("deleteHome", _) => return Err("deleteHome must be true or false".into()),
            (key, _) => return Err(format!("unknown query parameter {key}")),
        }
    }
    Ok(delete_home)
}

fn error(stream: &mut TcpStream, status: u16, code: &str, message: &str, details: Value) -> anyhow::Result<()> {
    let reason = match status {
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Internal Server Error",
    };
    super::server::write_v2_error(stream, status, reason, code, message, status >= 500, details)
}

fn bad_request(stream: &mut TcpStream, message: &str) -> anyhow::Result<()> {
    error(stream, 400, "invalid_request", message, json!({}))
}

fn not_found(stream: &mut TcpStream, id: &str) -> anyhow::Result<()> {
    error(stream, 404, "claude_account_not_found", &format!("no Claude account {id}"), json!({ "id": id }))
}

fn failed(stream: &mut TcpStream, err: &anyhow::Error) -> anyhow::Result<()> {
    error(stream, 500, "claude_accounts_failed", &format!("{err:#}"), json!({}))
}

fn item(id: &str) -> anyhow::Result<Option<Value>> {
    let registry = claude_accounts::load()?;
    if id != DEFAULT_ACCOUNT_ID && registry.find(id).is_none() {
        return Ok(None);
    }
    let usage = claude_accounts::usage_by_account(&registry);
    Ok(claude_accounts::item_json(&registry, id, usage.get(id)))
}

fn write_item(stream: &mut TcpStream, status: u16, id: &str) -> anyhow::Result<()> {
    match item(id) {
        Ok(Some(value)) => write_json_response(stream, status, if status == 201 { "Created" } else { "OK" }, &value),
        Ok(None) => not_found(stream, id),
        Err(err) => failed(stream, &err),
    }
}

fn body_object(request: &SimpleHttpRequest, allowed: &[&str]) -> Result<Map<String, Value>, String> {
    let value: Value = serde_json::from_slice(&request.body).map_err(|_| "body must be a JSON object".to_string())?;
    let Value::Object(object) = value else {
        return Err("body must be a JSON object".into());
    };
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(format!("unknown field {key} (allowed: {})", allowed.join(", ")));
    }
    Ok(object)
}

fn string_field(object: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match object.get(key) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(format!("{key}: must be a string")),
    }
}

fn key_env_field(object: &Map<String, Value>) -> Result<Option<ApiKeyEnv>, String> {
    string_field(object, "keyEnv")?
        .map(|text| {
            ApiKeyEnv::parse(&text).ok_or_else(|| "keyEnv: must be ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN".to_string())
        })
        .transpose()
}

fn env_field(object: &Map<String, Value>) -> Result<Option<BTreeMap<String, String>>, String> {
    match object.get("env") {
        None => Ok(None),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(key, value)| match value {
                Value::String(text) => Ok((key.clone(), text.clone())),
                _ => Err(format!("env.{key}: must be a string")),
            })
            .collect::<Result<BTreeMap<_, _>, _>>()
            .map(Some),
        Some(_) => Err("env: must be an object of strings".into()),
    }
}

const CREATE_FIELDS: &[&str] = &["label", "kind", "baseUrl", "keyEnv", "env", "apiKey"];
const PATCH_FIELDS: &[&str] = &["label", "baseUrl", "keyEnv", "env", "apiKey"];

/// Parse and validate `POST /v2/claude/accounts`.
fn parse_create(request: &SimpleHttpRequest) -> Result<NewClaudeAccount, String> {
    let object = body_object(request, CREATE_FIELDS)?;
    let label = string_field(&object, "label")?.ok_or("label is required")?;
    let label = claude_accounts::validate_label(&label).map_err(|e| format!("label: {e}"))?;
    let kind = match string_field(&object, "kind")? {
        None => ClaudeAccountKind::Subscription,
        Some(text) => ClaudeAccountKind::parse(&text).ok_or("kind: must be subscription or api")?,
    };
    let base_url = string_field(&object, "baseUrl")?;
    let key_env = key_env_field(&object)?;
    let env = env_field(&object)?;
    let api_key = string_field(&object, "apiKey")?
        .map(|key| claude_accounts::validate_api_key(&key).map_err(|e| format!("apiKey: {e}")))
        .transpose()?;
    let api = match kind {
        ClaudeAccountKind::Subscription => {
            if base_url.is_some() || key_env.is_some() || env.is_some() || api_key.is_some() {
                return Err("baseUrl, keyEnv, env and apiKey apply to kind \"api\" only".into());
            }
            None
        }
        ClaudeAccountKind::Api => {
            let api = ClaudeApiSettings {
                base_url: base_url.map(|url| url.trim().to_string()),
                key_env: key_env.unwrap_or_default(),
                env: env.unwrap_or_default(),
            };
            api.validate().map_err(|e| format!("{e:#}"))?;
            Some(api)
        }
    };
    Ok(NewClaudeAccount { label, kind, api, api_key })
}

/// Parse and validate `PATCH /v2/claude/accounts/{id}`.
fn parse_patch(request: &SimpleHttpRequest) -> Result<AccountPatch, String> {
    let object = body_object(request, PATCH_FIELDS)?;
    let label = string_field(&object, "label")?
        .map(|label| claude_accounts::validate_label(&label).map_err(|e| format!("label: {e}")))
        .transpose()?;
    let base_url = match object.get("baseUrl") {
        None => Change::Unchanged,
        Some(Value::Null) => Change::Clear,
        Some(Value::String(url)) => {
            claude_accounts::validate_base_url(url).map_err(|e| format!("baseUrl: {e}"))?;
            Change::Set(url.trim().to_string())
        }
        Some(_) => return Err("baseUrl: must be a string or null".into()),
    };
    let api_key = match object.get("apiKey") {
        None => None,
        Some(Value::String(key)) => {
            Some(claude_accounts::validate_api_key(key).map_err(|e| format!("apiKey: {e}"))?)
        }
        Some(_) => return Err("apiKey: must be a string (clearing is not supported)".into()),
    };
    Ok(AccountPatch { label, base_url, key_env: key_env_field(&object)?, env: env_field(&object)?, api_key })
}

fn label_taken(registry: &claude_accounts::ClaudeAccountsFile, label: &str, except: Option<&str>) -> bool {
    registry
        .accounts
        .iter()
        .any(|account| Some(account.id.as_str()) != except && account.label == label)
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let Some(route) = route(&request.path) else {
        return error(stream, 404, "not_found", "unknown Claude accounts route", json!({}));
    };
    let registry = match claude_accounts::load() {
        Ok(registry) => registry,
        Err(err) => return failed(stream, &err),
    };
    let known = |id: &str| id == DEFAULT_ACCOUNT_ID || registry.find(id).is_some();
    match (request.method.as_str(), route) {
        ("GET", Route::Collection) => match claude_accounts::list_json() {
            Ok(value) => write_json_response(stream, 200, "OK", &value),
            Err(err) => failed(stream, &err),
        },
        ("POST", Route::Collection) => {
            let create = match parse_create(request) {
                Ok(create) => create,
                Err(message) => return bad_request(stream, &message),
            };
            if label_taken(&registry, &create.label, None) {
                return error(
                    stream,
                    409,
                    "claude_account_label_taken",
                    &format!("a Claude account labelled {} already exists", create.label),
                    json!({ "label": create.label }),
                );
            }
            match claude_accounts::add(create) {
                Ok(account) => write_item(stream, 201, &account.id),
                Err(err) => failed(stream, &err),
            }
        }
        ("GET", Route::Item(id)) => {
            if !known(id) {
                return not_found(stream, id);
            }
            write_item(stream, 200, id)
        }
        ("PATCH", Route::Item(id)) => {
            if id == DEFAULT_ACCOUNT_ID {
                return error(
                    stream,
                    409,
                    "claude_account_is_default",
                    "the default Claude account has no editable settings",
                    json!({ "id": id }),
                );
            }
            let Some(account) = registry.find(id) else {
                return not_found(stream, id);
            };
            let patch = match parse_patch(request) {
                Ok(patch) => patch,
                Err(message) => return bad_request(stream, &message),
            };
            if account.kind != ClaudeAccountKind::Api
                && (patch.base_url != Change::Unchanged
                    || patch.key_env.is_some()
                    || patch.env.is_some()
                    || patch.api_key.is_some())
            {
                return error(
                    stream,
                    409,
                    "claude_account_kind_mismatch",
                    "baseUrl, keyEnv, env and apiKey apply to API accounts only",
                    json!({ "id": id, "kind": account.kind.as_str() }),
                );
            }
            if let Some(label) = &patch.label {
                if label_taken(&registry, label, Some(id)) {
                    return error(
                        stream,
                        409,
                        "claude_account_label_taken",
                        &format!("a Claude account labelled {label} already exists"),
                        json!({ "label": label }),
                    );
                }
            }
            match claude_accounts::update(id, patch) {
                Ok(_) => write_item(stream, 200, id),
                Err(err) => bad_request(stream, &format!("{err:#}")),
            }
        }
        ("DELETE", Route::Item(id)) => {
            let delete_home = match delete_home_requested(&request.path) {
                Ok(delete_home) => delete_home,
                Err(message) => return bad_request(stream, &message),
            };
            match claude_accounts::remove(id, delete_home) {
                Ok(outcome) => write_json_response(
                    stream,
                    200,
                    "OK",
                    &json!({
                        "id": outcome.removed.id,
                        "removed": true,
                        "homeDeleted": outcome.home_deleted,
                        "keptHome": outcome.kept_home,
                    }),
                ),
                Err(RemoveError::IsDefault) => error(
                    stream,
                    409,
                    "claude_account_is_default",
                    "the default Claude account cannot be removed",
                    json!({ "id": id }),
                ),
                Err(RemoveError::NotFound) => not_found(stream, id),
                Err(err @ RemoveError::InUse(_)) => {
                    let RemoveError::InUse(usage) = &err else { unreachable!() };
                    let details = json!({
                        "id": id,
                        "profiles": usage.profiles,
                        "sessions": usage.sessions.iter().filter(|s| s.lifecycle == "active").collect::<Vec<_>>(),
                    });
                    error(stream, 409, "claude_account_in_use", &err.to_string(), details)
                }
                Err(RemoveError::Other(err)) => failed(stream, &err),
            }
        }
        ("POST", Route::Status(id)) => {
            if !known(id) {
                return not_found(stream, id);
            }
            match claude_accounts::check_status(id) {
                Ok(_) => write_item(stream, 200, id),
                Err(err) => failed(stream, &err),
            }
        }
        ("POST", Route::Logout(id)) => {
            if !known(id) {
                return not_found(stream, id);
            }
            if registry.find(id).is_some_and(|account| account.kind == ClaudeAccountKind::Api) {
                return error(
                    stream,
                    409,
                    "claude_account_kind_mismatch",
                    "API accounts have no login",
                    json!({ "id": id, "kind": "api" }),
                );
            }
            match claude_accounts::run_logout(id) {
                Ok(_) => write_item(stream, 200, id),
                Err(err) => failed(stream, &err),
            }
        }
        (method, _) => super::server::write_v2_error(
            stream,
            405,
            "Method Not Allowed",
            "method_not_allowed",
            &format!("{method} is not allowed on this Claude accounts route"),
            false,
            json!({}),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(body: &str) -> SimpleHttpRequest {
        SimpleHttpRequest {
            method: "POST".into(),
            path: PATH.into(),
            headers: Default::default(),
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn routes_accept_default_and_account_ids_only() {
        assert_eq!(route("/v2/claude/accounts"), Some(Route::Collection));
        assert_eq!(route("/v2/claude/accounts?x=1"), Some(Route::Collection));
        assert_eq!(route("/v2/claude/accounts/default"), Some(Route::Item("default")));
        assert_eq!(route("/v2/claude/accounts/ca-abc123/status"), Some(Route::Status("ca-abc123")));
        assert_eq!(route("/v2/claude/accounts/ca-abc123/logout"), Some(Route::Logout("ca-abc123")));
        assert_eq!(route("/v2/claude/accounts/ca-abc123?deleteHome=true"), Some(Route::Item("ca-abc123")));
        assert_eq!(route("/v2/claude/accounts/../x"), None);
        assert_eq!(route("/v2/claude/accounts/CA-X"), None);
        assert_eq!(route("/v2/claude/accounts/ca-abc123/login"), None);
        assert!(is_path("/v2/claude/accounts/ca-x") && !is_path("/v2/claude/accountsx"));
        assert_eq!(delete_home_requested("/v2/claude/accounts/ca-a?deleteHome=true"), Ok(true));
        assert_eq!(delete_home_requested("/v2/claude/accounts/ca-a"), Ok(false));
        assert!(delete_home_requested("/v2/claude/accounts/ca-a?deleteHome=yes").is_err());
    }

    #[test]
    fn create_bodies_are_validated_before_anything_is_written() {
        let subscription = parse_create(&request(r#"{"label":"Work"}"#)).unwrap();
        assert_eq!(subscription.kind, ClaudeAccountKind::Subscription);
        assert!(subscription.api.is_none() && subscription.api_key.is_none());

        let api = parse_create(&request(
            r#"{"label":"DeepSeek","kind":"api","baseUrl":"https://api.deepseek.com/anthropic",
                "env":{"ANTHROPIC_DEFAULT_SONNET_MODEL":"deepseek-v4-pro"},"apiKey":"sk-test-only"}"#,
        ))
        .unwrap();
        let settings = api.api.as_ref().unwrap();
        assert_eq!(settings.base_url.as_deref(), Some("https://api.deepseek.com/anthropic"));
        assert_eq!(settings.key_env, ApiKeyEnv::ApiKey);
        assert_eq!(api.api_key.as_deref(), Some("sk-test-only"));
        // The key never shows in debug output.
        assert!(!format!("{api:?}").contains("sk-test-only"));

        for body in [
            r#"{}"#,
            r#"{"label":"default"}"#,
            r#"{"label":"x","kind":"team"}"#,
            r#"{"label":"x","apiKey":"k"}"#,
            r#"{"label":"x","baseUrl":"https://a.example"}"#,
            r#"{"label":"x","kind":"api","baseUrl":"ftp://a.example"}"#,
            r#"{"label":"x","kind":"api","baseUrl":"https://u:p@a.example"}"#,
            r#"{"label":"x","kind":"api","env":{"ANTHROPIC_API_KEY":"k"}}"#,
            r#"{"label":"x","kind":"api","env":{"ANTHROPIC_BASE_URL":"https://a.example"}}"#,
            r#"{"label":"x","kind":"api","keyEnv":"OPENAI_API_KEY"}"#,
            r#"{"label":"x","kind":"api","apiKey":"has space"}"#,
            r#"{"label":"x","home":"C:/elsewhere"}"#,
        ] {
            assert!(parse_create(&request(body)).is_err(), "{body}");
        }
    }

    #[test]
    fn patch_bodies_are_tri_state() {
        let patch = parse_patch(&request(r#"{"baseUrl":null,"keyEnv":"ANTHROPIC_AUTH_TOKEN"}"#)).unwrap();
        assert_eq!(patch.base_url, Change::Clear);
        assert_eq!(patch.key_env, Some(ApiKeyEnv::AuthToken));
        assert!(patch.label.is_none() && patch.env.is_none() && patch.api_key.is_none());
        let patch = parse_patch(&request(r#"{"label":"Personal"}"#)).unwrap();
        assert_eq!(patch.label.as_deref(), Some("Personal"));
        assert_eq!(patch.base_url, Change::Unchanged);
        assert!(parse_patch(&request(r#"{"apiKey":null}"#)).is_err());
        assert!(parse_patch(&request(r#"{"kind":"api"}"#)).is_err());
    }
}
