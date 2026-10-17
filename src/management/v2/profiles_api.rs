//! Root-only profile settings routes:
//! `GET|POST /v2/profiles`, `GET|PATCH|DELETE /v2/profiles/{profile}`,
//! `POST /v2/profiles/{profile}/duplicate`, `GET /v2/profile-presets`,
//! `POST /v2/profiles/{profile}/export`, `POST /v2/profiles/import`
//! (`profile-transfer`; the export may carry an API key, loopback only).
//! `{profile}` is a profile ID or exact name (percent-encoded). Responses are
//! camelCase and secret-free; `apiKey` is write-only.
//!
//! Create / duplicate / delete / rename go through
//! `crate::profiles::lifecycle` (the CLI's profile layout).

use std::net::TcpStream;

use serde_json::{json, Map, Value};

use crate::config::global_settings::ConfigValueUpdate;
use crate::config::store::load_codez_config;
use crate::http::server::write_json_response;
use crate::http::server::SimpleHttpRequest;
use crate::profiles::appearance::AgentTurnQuestions;
use crate::profiles::codex_profile::{CodexProfileConfigPatch, MODEL_REASONING_SUMMARY_VALUES, TOOL_SCHEMA_COMPAT_VALUES, WEB_SEARCH_VALUES};
use crate::profiles::lifecycle::{self, LifecycleError, NewProfile, NewProfileKind};
use crate::profiles::settings_api::{self, ApiKey, ProfileApiPatch, MAX_API_KEY_BYTES};
use crate::profiles::transfer;

pub const PRESETS_PATH: &str = "/v2/profile-presets";

pub fn is_list_path(path: &str) -> bool {
    path == "/v2/profiles"
}

fn decode_profile_segment(encoded: &str) -> Option<String> {
    if encoded.is_empty() || encoded.contains('/') {
        return None;
    }
    let decoded = super::profile_quota::percent_decode(encoded)?;
    (!decoded.is_empty() && !decoded.contains('/') && !decoded.chars().any(char::is_control))
        .then_some(decoded)
}

/// `{profile}` for `/v2/profiles/{profile}` (exactly one segment).
pub fn parse_item_path(path: &str) -> Option<String> {
    decode_profile_segment(path.strip_prefix("/v2/profiles/")?)
}

/// `{profile}` for `/v2/profiles/{profile}/duplicate`.
pub fn parse_duplicate_path(path: &str) -> Option<String> {
    decode_profile_segment(path.strip_prefix("/v2/profiles/")?.strip_suffix("/duplicate")?)
}

/// `{profile}` for `/v2/profiles/{profile}/export` (`profile-transfer`).
pub fn parse_export_path(path: &str) -> Option<String> {
    decode_profile_segment(path.strip_prefix("/v2/profiles/")?.strip_suffix("/export")?)
}

/// `POST /v2/profiles/import` (`profile-transfer`). The same path with
/// another method still addresses a profile named `import`.
pub const IMPORT_PATH: &str = "/v2/profiles/import";

pub fn is_profiles_path(path: &str) -> bool {
    is_list_path(path)
        || path == PRESETS_PATH
        || parse_item_path(path).is_some()
        || parse_duplicate_path(path).is_some()
        || parse_export_path(path).is_some()
}

fn profile_body(account: &crate::profiles::model::StoredAccount) -> Value {
    json!({
        "contractVersion": 2,
        "profile": settings_api::profile_item(account, &load_codez_config(), true),
    })
}

fn lifecycle_error(stream: &mut TcpStream, profile: &str, failure: LifecycleError) -> anyhow::Result<()> {
    let message = failure.to_string();
    let (status, reason, code, retryable, details) = match failure {
        LifecycleError::Invalid(_) => (400, "Bad Request", "invalid_request", false, json!({})),
        LifecycleError::NotFound => return not_found(stream, profile),
        LifecycleError::NameConflict(name) => {
            (409, "Conflict", "profile_name_conflict", false, json!({ "name": name }))
        }
        LifecycleError::InUse(sessions) => (
            409,
            "Conflict",
            "profile_in_use",
            false,
            json!({ "profile": profile, "sessions": sessions }),
        ),
        LifecycleError::IsDefault(name) => {
            (409, "Conflict", "profile_is_default", false, json!({ "profile": name }))
        }
        LifecycleError::Store(error) => (
            503,
            "Service Unavailable",
            "profile_store_unavailable",
            true,
            json!({ "diagnostic": format!("{error:#}") }),
        ),
    };
    super::server::write_v2_error(stream, status, reason, code, &message, retryable, details)
}

fn bad_request(stream: &mut TcpStream, message: &str) -> anyhow::Result<()> {
    super::server::write_v2_error(
        stream,
        400,
        "Bad Request",
        "invalid_request",
        message,
        false,
        json!({}),
    )
}

fn store_unavailable(stream: &mut TcpStream, error: &anyhow::Error) -> anyhow::Result<()> {
    super::server::write_v2_error(
        stream,
        503,
        "Service Unavailable",
        "profile_store_unavailable",
        "the profile store could not be read",
        true,
        json!({ "diagnostic": format!("{error:#}") }),
    )
}

pub fn list_resource() -> anyhow::Result<Value> {
    let store = settings_api::load_store()?;
    let config = load_codez_config();
    Ok(json!({
        "contractVersion": 2,
        "defaultProfile": config.default_profile,
        "profiles": store
            .accounts
            .iter()
            .map(|account| settings_api::profile_item(account, &config, false))
            .collect::<Vec<_>>(),
    }))
}

pub(super) fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest, path: &str) -> anyhow::Result<()> {
    if path == PRESETS_PATH {
        if request.method != "GET" {
            return method_not_allowed(stream, "use GET for /v2/profile-presets");
        }
        return write_json_response(
            stream,
            200,
            "OK",
            &json!({ "contractVersion": 2, "presets": crate::profiles::providers::presets_json() }),
        );
    }
    if is_list_path(path) {
        return match request.method.as_str() {
            "GET" => match list_resource() {
                Ok(value) => write_json_response(stream, 200, "OK", &value),
                Err(error) => store_unavailable(stream, &error),
            },
            "POST" => {
                let create = match parse_create(&request.body) {
                    Ok(create) => create,
                    Err(message) => return bad_request(stream, &message),
                };
                let claude_account = match parse_create_claude_account(&request.body) {
                    Ok(account) => account,
                    Err(message) => return bad_request(stream, &message),
                };
                if let Some(id) = &claude_account {
                    match crate::runtime::claude_accounts::load() {
                        Ok(registry) if registry.find(id).is_some() => {}
                        Ok(_) => return bad_request(stream, &format!("claudeAccount: no Claude account {id} on this host")),
                        Err(error) => return store_unavailable(stream, &error),
                    }
                }
                match lifecycle::create_profile(&create) {
                    Ok(account) => {
                        let account = match &claude_account {
                            None => account,
                            Some(id) => {
                                let patch = settings_api::ProfileApiPatch {
                                    claude_account: ConfigValueUpdate::Set(id.clone()),
                                    ..Default::default()
                                };
                                match settings_api::update_profile(&account.id, &patch) {
                                    Ok((account, _)) => account,
                                    Err(error) => {
                                        let _ = lifecycle::delete_profile(&account.id);
                                        return bad_request(stream, &format!("{error:#}"));
                                    }
                                }
                            }
                        };
                        let mut body = profile_body(&account);
                        body["created"] = json!(true);
                        body["kind"] = json!(create.kind.as_str());
                        body["credentialsPresent"] = json!(create.api_key.is_some());
                        write_json_response(stream, 201, "Created", &body)
                    }
                    Err(failure) => lifecycle_error(stream, &create.name, failure),
                }
            }
            _ => method_not_allowed(stream, "use GET or POST for /v2/profiles"),
        };
    }
    if let Some(profile) = parse_export_path(path) {
        if request.method != "POST" {
            return method_not_allowed(stream, "use POST for /v2/profiles/{profile}/export");
        }
        return handle_export(stream, &request.body, &profile);
    }
    if path == IMPORT_PATH && request.method == "POST" {
        return handle_import(stream, &request.body);
    }
    if let Some(profile) = parse_duplicate_path(path) {
        if request.method != "POST" {
            return method_not_allowed(stream, "use POST for /v2/profiles/{profile}/duplicate");
        }
        let name = match parse_name_body(&request.body) {
            Ok(name) => name,
            Err(message) => return bad_request(stream, &message),
        };
        return match lifecycle::duplicate_profile(&profile, &name) {
            Ok(account) => {
                let mut body = profile_body(&account);
                body["duplicatedFrom"] = json!(profile);
                // Credentials (auth.json, API key, Claude login) are never copied.
                body["credentialsCopied"] = json!(false);
                write_json_response(stream, 201, "Created", &body)
            }
            Err(failure) => lifecycle_error(stream, &profile, failure),
        };
    }
    let profile = parse_item_path(path).expect("matched profile item path");
    match request.method.as_str() {
        "GET" => {
            let store = match settings_api::load_store() {
                Ok(store) => store,
                Err(error) => return store_unavailable(stream, &error),
            };
            let Some(account) = settings_api::find(&store, &profile) else {
                return not_found(stream, &profile);
            };
            let item = settings_api::profile_item(account, &load_codez_config(), true);
            write_json_response(stream, 200, "OK", &json!({ "contractVersion": 2, "profile": item }))
        }
        "PATCH" => {
            let parsed = parse_name_field(&request.body)
                .and_then(|name| Ok((name, parse_patch(&request.body)?)));
            let (rename, patch) = match parsed {
                Ok(parsed) => parsed,
                Err(message) => return bad_request(stream, &message),
            };
            let account_id = match settings_api::load_store() {
                Ok(store) => match settings_api::find(&store, &profile) {
                    Some(account) => account.id.clone(),
                    None => return not_found(stream, &profile),
                },
                Err(error) => return store_unavailable(stream, &error),
            };
            let renamed = match &rename {
                Some(name) => match lifecycle::rename_profile(&account_id, name) {
                    Ok(outcome) => outcome.changed.then(|| (outcome.old_name, outcome.account.name)),
                    Err(failure) => return lifecycle_error(stream, &profile, failure),
                },
                None => None,
            };
            match settings_api::update_profile(&account_id, &patch) {
                Ok((account, changed)) => {
                    let item = settings_api::profile_item(&account, &load_codez_config(), true);
                    write_json_response(
                        stream,
                        200,
                        "OK",
                        &json!({
                            "contractVersion": 2,
                            "changed": changed || renamed.is_some(),
                            "renamed": renamed.as_ref().map(|(from, to)| json!({ "from": from, "to": to })),
                            "appliesTo": "newRuntimes",
                            "profile": item,
                        }),
                    )
                }
                Err(error) => {
                    // Keep PATCH all-or-nothing: undo the rename.
                    if let Some((from, _)) = &renamed {
                        if let Err(revert) = lifecycle::rename_profile(&account_id, from) {
                            eprintln!("profile PATCH: could not revert rename of {account_id} to {from}: {revert}");
                        }
                    }
                    bad_request(stream, &format!("{error:#}"))
                }
            }
        }
        "DELETE" => {
            let archived = match archived_references(&request.path) {
                Ok(archived) => archived,
                Err(message) => return bad_request(stream, &message),
            };
            match lifecycle::delete_profile_with(&profile, archived) {
            Ok(outcome) => write_json_response(
                stream,
                200,
                "OK",
                &json!({
                    "contractVersion": 2,
                    "deleted": { "id": outcome.removed.id, "name": outcome.removed.name },
                    "backupPath": outcome.backup_path.map(|path| path.display().to_string()),
                    "backupError": outcome.backup_error,
                    "clearedReferences": {
                        "globalConfig": outcome.references.global_config_changed,
                        "quickRunState": outcome.references.quick_state_changed,
                    },
                    "detachedSessions": outcome.detached_sessions,
                }),
            ),
            Err(failure) => lifecycle_error(stream, &profile, failure),
            }
        }
        _ => method_not_allowed(stream, "use GET, PATCH or DELETE for /v2/profiles/{profile}"),
    }
}

/// `DELETE /v2/profiles/{p}?archived=detach|refuse` (default refuse; see
/// `lifecycle::ArchivedReferences`).
fn archived_references(path: &str) -> Result<lifecycle::ArchivedReferences, String> {
    let Some((_, query)) = path.split_once('?') else {
        return Ok(lifecycle::ArchivedReferences::Refuse);
    };
    let mut archived = lifecycle::ArchivedReferences::Refuse;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        match pair.split_once('=').unwrap_or((pair, "")) {
            ("archived", "detach") => archived = lifecycle::ArchivedReferences::Detach,
            ("archived", "refuse") => archived = lifecycle::ArchivedReferences::Refuse,
            ("archived", _) => return Err("archived must be detach or refuse".into()),
            (key, _) => return Err(format!("unknown query parameter {key}")),
        }
    }
    Ok(archived)
}

fn transfer_error(stream: &mut TcpStream, profile: &str, failure: transfer::TransferError) -> anyhow::Result<()> {
    use transfer::TransferError;
    // Messages are fixed texts or file diagnostics: never bundle values.
    let message = failure.to_string();
    let (status, reason, code, retryable, details) = match failure {
        TransferError::NotFound => return not_found(stream, profile),
        TransferError::Invalid(_) => (400, "Bad Request", "invalid_request", false, json!({})),
        TransferError::Unsupported(_) => (
            400,
            "Bad Request",
            "unsupported_bundle",
            false,
            json!({ "schemaVersion": transfer::BUNDLE_SCHEMA_VERSION }),
        ),
        TransferError::NameConflict(name) => {
            (409, "Conflict", "profile_name_conflict", false, json!({ "name": name }))
        }
        TransferError::Store(error) => (
            503,
            "Service Unavailable",
            "profile_store_unavailable",
            true,
            json!({ "diagnostic": format!("{error:#}") }),
        ),
    };
    super::server::write_v2_error(stream, status, reason, code, &message, retryable, details)
}

/// `POST /v2/profiles/{p}/export {includeCredentials?: bool}`. Credentials
/// only over a loopback connection (an SSH tunnel arrives on loopback; a
/// tailnet / LAN listener is refused with 403 `credentials_loopback_only`).
/// Audited without values. POST only: the bundle must not be cached.
fn handle_export(stream: &mut TcpStream, body: &[u8], profile: &str) -> anyhow::Result<()> {
    let include_credentials = match parse_export_body(body) {
        Ok(include) => include,
        Err(message) => return bad_request(stream, &message),
    };
    let loopback = crate::management::listeners::current_request_is_loopback();
    if include_credentials && !loopback {
        transfer::audit(
            "profileExportRefused",
            json!({
                "profile": profile,
                "includeCredentials": true,
                "reason": "credentials_loopback_only",
                "actor": crate::management::users::current_actor(),
            }),
        );
        return super::server::write_v2_error(
            stream,
            403,
            "Forbidden",
            "credentials_loopback_only",
            "credentials can only be exported over a loopback connection (this machine or an SSH tunnel); export without credentials or connect through SSH",
            false,
            json!({ "profile": profile }),
        );
    }
    match transfer::export_profile(profile, include_credentials) {
        Ok(export) => {
            transfer::audit(
                "profileExport",
                json!({
                    "profileId": export.profile_id,
                    "profileName": export.profile_name,
                    "includeCredentials": include_credentials,
                    "credentialsIncluded": export.credentials_included,
                    "credentialKind": export.credential_kind.as_str(),
                    "loopback": loopback,
                    "actor": crate::management::users::current_actor(),
                }),
            );
            write_json_response(
                stream,
                200,
                "OK",
                &json!({
                    "contractVersion": 2,
                    "credentialsIncluded": export.credentials_included,
                    "credentialKind": export.credential_kind.as_str(),
                    "bundle": export.bundle,
                }),
            )
        }
        Err(failure) => transfer_error(stream, profile, failure),
    }
}

/// `POST /v2/profiles/import {bundle, name, onConflict?: "fail"}` (201).
fn handle_import(stream: &mut TcpStream, body: &[u8]) -> anyhow::Result<()> {
    let (bundle, name) = match parse_import_body(body) {
        Ok(parsed) => parsed,
        Err(message) => return bad_request(stream, &message),
    };
    match transfer::import_profile(&bundle, &name) {
        Ok(outcome) => {
            transfer::audit(
                "profileImport",
                json!({
                    "profileId": outcome.account.id,
                    "profileName": outcome.account.name,
                    "sourceProfileName": outcome.source_name,
                    "credentialsCopied": outcome.credentials_copied,
                    "credentialKind": outcome.credential_kind.as_str(),
                    "loopback": crate::management::listeners::current_request_is_loopback(),
                    "actor": crate::management::users::current_actor(),
                }),
            );
            let mut response = profile_body(&outcome.account);
            response["imported"] = json!(true);
            response["credentialKind"] = json!(outcome.credential_kind.as_str());
            response["credentialsCopied"] = json!(outcome.credentials_copied);
            response["warnings"] = json!(outcome.warnings);
            write_json_response(stream, 201, "Created", &response)
        }
        Err(failure) => transfer_error(stream, &name, failure),
    }
}

pub(super) fn parse_export_body(body: &[u8]) -> Result<bool, String> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(false);
    }
    let body = object_body(body)?;
    if let Some(unknown) = body.keys().find(|key| key.as_str() != "includeCredentials") {
        return Err(format!("unsupported field: {unknown}"));
    }
    Ok(optional_bool(&body, "includeCredentials")?.unwrap_or(false))
}

pub(super) fn parse_import_body(body: &[u8]) -> Result<(Value, String), String> {
    // No serde text: a malformed body may hold an API key.
    let value: Value = serde_json::from_slice(body).map_err(|_| "JSON object body required".to_string())?;
    let Value::Object(mut body) = value else {
        return Err("JSON object body required".into());
    };
    if let Some(unknown) = body.keys().find(|key| !["bundle", "name", "onConflict"].contains(&key.as_str())) {
        return Err(format!("unsupported field: {unknown}"));
    }
    match body.get("onConflict") {
        None | Some(Value::Null) => {}
        Some(Value::String(mode)) if mode == "fail" => {}
        Some(_) => return Err("onConflict: only \"fail\" is supported (imports never overwrite)".into()),
    }
    let name = name_value(&body)?.ok_or_else(|| "name: required string".to_string())?;
    let bundle = match body.remove("bundle") {
        Some(bundle @ Value::Object(_)) => bundle,
        _ => return Err("bundle: required object (from POST /v2/profiles/{profile}/export)".into()),
    };
    Ok((bundle, name))
}

const CREATE_FIELDS: &[&str] =&["name", "kind", "preset", "baseUrl", "model", "apiKey", "claudeAccount"];

fn object_body(body: &[u8]) -> Result<Map<String, Value>, String> {
    let value: Value =
        serde_json::from_slice(body).map_err(|error| format!("JSON object body required: {error}"))?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Err("JSON object body required".into()),
    }
}

fn optional<T>(
    body: &Map<String, Value>,
    key: &str,
    parse: impl FnOnce(&Value) -> Result<T, String>,
) -> Result<Option<T>, String> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => parse(value).map(Some).map_err(|message| format!("{key}: {message}")),
    }
}

fn api_key_value(value: &Value) -> Result<ApiKey, String> {
    let key = value.as_str().ok_or_else(|| "must be a string".to_string())?.trim();
    if key.is_empty()
        || key.len() > MAX_API_KEY_BYTES
        || key.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(format!("must be 1-{MAX_API_KEY_BYTES} non-whitespace characters"));
    }
    Ok(ApiKey(key.to_string()))
}

/// `POST /v2/profiles` body. `kind` defaults to `apiKey` (with `preset`
/// defaulting to `custom`), so the legacy `{name}` body creates an empty
/// custom API-key profile to fill in afterwards.
pub(super) fn parse_create(body: &[u8]) -> Result<NewProfile, String> {
    let body = object_body(body)?;
    if let Some(unknown) = body.keys().find(|key| !CREATE_FIELDS.contains(&key.as_str())) {
        return Err(format!("unsupported field: {unknown}"));
    }
    let name = match body.get("name") {
        Some(Value::String(name)) => {
            lifecycle::validate_profile_name(name).map_err(|message| format!("name: {message}"))?
        }
        _ => return Err("name: required string".into()),
    };
    let kind = optional(&body, "kind", |value| {
        value
            .as_str()
            .and_then(NewProfileKind::parse)
            .ok_or_else(|| "must be one of chatgpt, claude, apiKey".to_string())
    })?
    .unwrap_or(NewProfileKind::ApiKey);
    let preset = optional(&body, "preset", |value| {
        let id = value.as_str().ok_or("must be a string")?;
        crate::profiles::providers::find_preset(id).ok_or_else(|| {
            format!(
                "unknown preset (known: {})",
                crate::profiles::providers::API_KEY_PRESETS
                    .iter()
                    .map(|preset| preset.id)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
    })?;
    let base_url = optional(&body, "baseUrl", http_url)?;
    let model = optional(&body, "model", visible_string(200))?;
    let api_key = optional(&body, "apiKey", api_key_value)?;
    if kind != NewProfileKind::ApiKey && (preset.is_some() || base_url.is_some() || api_key.is_some()) {
        return Err(format!(
            "preset, baseUrl and apiKey are only valid for kind \"apiKey\" (got \"{}\")",
            kind.as_str()
        ));
    }
    if kind != NewProfileKind::Claude && body.get("claudeAccount").is_some_and(|value| !value.is_null()) {
        return Err(format!("claudeAccount is only valid for kind \"claude\" (got \"{}\")", kind.as_str()));
    }
    Ok(NewProfile { name, kind, preset, base_url, model, api_key })
}

/// `claudeAccount` of a `POST /v2/profiles` body (kind `claude`; checked by
/// [`parse_create`]); `None` / `default` = the shared default account.
pub(super) fn parse_create_claude_account(body: &[u8]) -> Result<Option<String>, String> {
    let body = object_body(body)?;
    Ok(optional(&body, "claudeAccount", claude_account_id)?
        .filter(|id| id != crate::runtime::claude_accounts::DEFAULT_ACCOUNT_ID))
}

fn name_value(body: &Map<String, Value>) -> Result<Option<String>, String> {
    match body.get("name") {
        None => Ok(None),
        Some(Value::String(name)) => lifecycle::validate_profile_name(name)
            .map(Some)
            .map_err(|message| format!("name: {message}")),
        Some(_) => Err("name: must be a string".into()),
    }
}

/// `{name}` body (duplicate).
pub(super) fn parse_name_body(body: &[u8]) -> Result<String, String> {
    let body = object_body(body)?;
    if let Some(unknown) = body.keys().find(|key| key.as_str() != "name") {
        return Err(format!("unsupported field: {unknown}"));
    }
    name_value(&body)?.ok_or_else(|| "name: required string".into())
}

/// Optional `name` (rename) in a PATCH body.
pub(super) fn parse_name_field(body: &[u8]) -> Result<Option<String>, String> {
    name_value(&object_body(body)?)
}

/// 404 `profile_not_found`; the message and `details.availableProfiles` list
/// the current names (an old name no longer resolves after a rename).
pub(super) fn not_found(stream: &mut TcpStream, profile: &str) -> anyhow::Result<()> {
    let available = crate::profiles::identity::current_profile_names();
    super::server::write_v2_error(
        stream,
        404,
        "Not Found",
        "profile_not_found",
        &crate::profiles::identity::unknown_profile_message(profile, &available),
        false,
        json!({ "profile": profile, "availableProfiles": available }),
    )
}

fn method_not_allowed(stream: &mut TcpStream, message: &str) -> anyhow::Result<()> {
    super::server::write_v2_error(
        stream,
        405,
        "Method Not Allowed",
        "method_not_allowed",
        message,
        false,
        json!({}),
    )
}

const PATCH_FIELDS: &[&str] = &[
    // Rename (handled by `parse_name_field` + `lifecycle::rename_profile`).
    "name",
    "model",
    "modelProvider",
    "forcedLoginMethod",
    "modelReasoningEffort",
    "modelReasoningSummary",
    "updatePlanEnabled",
    "webSearch",
    "toolSchemaCompat",
    "modelContextWindow",
    "modelAutoCompactTokenLimit",
    "providerName",
    "providerBaseUrl",
    "providerEnvKey",
    "providerWireApi",
    "requestMaxRetries",
    "streamMaxRetries",
    "streamIdleTimeoutMs",
    "websocketConnectTimeoutMs",
    "requiresOpenaiAuth",
    "supportsWebsockets",
    "providerInstructionsField",
    "color",
    "providerCategory",
    "allowSessionSwitch",
    "nativeSubagents",
    "delegationWait",
    "browserTools",
    "searchTool",
    "privateChatgptAuthFile",
    "agentUseRequiresApproval",
    "agentTurnQuestions",
    "claudeFileCheckpointing",
    "claudeAllowWrite",
    "claudeAllowExec",
    "planLabel",
    "claudeAccount",
    "modelDisplayNames",
    "apiKey",
];

/// Tri-state read: absent = Unchanged, `null` = Clear, value = Set.
pub(super) fn tri_state<T>(
    body: &Map<String, Value>,
    key: &str,
    parse: impl FnOnce(&Value) -> Result<T, String>,
) -> Result<ConfigValueUpdate<T>, String> {
    match body.get(key) {
        None => Ok(ConfigValueUpdate::Unchanged),
        Some(Value::Null) => Ok(ConfigValueUpdate::Clear),
        Some(value) => parse(value)
            .map(ConfigValueUpdate::Set)
            .map_err(|message| format!("{key}: {message}")),
    }
}

pub(super) fn visible_string(max: usize) -> impl Fn(&Value) -> Result<String, String> {
    move |value| {
        let text = value.as_str().ok_or("must be a string or null")?.trim();
        if text.is_empty() || text.len() > max || text.chars().any(char::is_control) {
            return Err(format!("must be 1-{max} visible characters"));
        }
        Ok(text.to_string())
    }
}

fn token(max: usize, extra: &'static str) -> impl Fn(&Value) -> Result<String, String> {
    move |value| {
        let text = value.as_str().ok_or("must be a string or null")?.trim();
        let valid = !text.is_empty()
            && text.len() <= max
            && text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || extra.contains(c));
        valid
            .then(|| text.to_string())
            .ok_or_else(|| format!("must be 1-{max} characters of [A-Za-z0-9{extra}]"))
    }
}

fn one_of(allowed: &'static [&'static str]) -> impl Fn(&Value) -> Result<String, String> {
    move |value| {
        let text = value.as_str().ok_or("must be a string or null")?.trim();
        allowed
            .contains(&text)
            .then(|| text.to_string())
            .ok_or_else(|| format!("must be one of {}", allowed.join(", ")))
    }
}

fn env_name(value: &Value) -> Result<String, String> {
    let text = value.as_str().ok_or("must be a string or null")?.trim();
    let valid = !text.is_empty()
        && text.len() <= 128
        && text.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && !text.starts_with(|c: char| c.is_ascii_digit());
    valid
        .then(|| text.to_string())
        .ok_or_else(|| "must be an environment variable name ([A-Z_][A-Z0-9_]*)".to_string())
}

fn bounded_count(max: u64) -> impl Fn(&Value) -> Result<u64, String> {
    move |value| {
        value
            .as_u64()
            .filter(|count| *count <= max)
            .ok_or_else(|| format!("must be an integer 0-{max} or null"))
    }
}

/// Absent = unchanged; only `true` / `false` are accepted.
fn optional_bool(body: &Map<String, Value>, key: &str) -> Result<Option<bool>, String> {
    match body.get(key) {
        None => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(format!("{key}: must be a boolean")),
    }
}

fn agent_turn_questions(value: &Value) -> Result<AgentTurnQuestions, String> {
    value
        .as_str()
        .and_then(AgentTurnQuestions::parse)
        .ok_or_else(|| "must be decide, wait or null".to_string())
}

/// `default` or an added account id (existence is checked on apply).
fn claude_account_id(value: &Value) -> Result<String, String> {
    let text = value.as_str().ok_or("must be a Claude account id, \"default\" or null")?.trim();
    if text == crate::runtime::claude_accounts::DEFAULT_ACCOUNT_ID {
        return Ok(text.to_string());
    }
    crate::runtime::claude_accounts::validate_id(text)
        .map(|()| text.to_string())
        .map_err(|_| "must be a Claude account id (ca-…), \"default\" or null".to_string())
}

/// `modelDisplayNames`: `{modelId: name | null}`, merged into the stored map
/// (`null` or "" removes that entry's name). Absent = unchanged.
fn model_display_names(
    body: &Map<String, Value>,
) -> Result<std::collections::BTreeMap<String, Option<String>>, String> {
    let Some(value) = body.get("modelDisplayNames") else {
        return Ok(Default::default());
    };
    let object = value
        .as_object()
        .ok_or("modelDisplayNames: must be an object {modelId: name | null}")?;
    if object.len() > crate::profiles::settings_api::MAX_MODEL_DISPLAY_NAMES {
        return Err("modelDisplayNames: too many entries".into());
    }
    object
        .iter()
        .map(|(id, name)| {
            crate::profiles::catalog_api::validate_model_id(id)
                .map_err(|error| format!("modelDisplayNames: {error}"))?;
            let name = match name {
                Value::Null => None,
                Value::String(text) => crate::profiles::settings_api::normalize_model_display_name(text)
                    .map_err(|message| format!("modelDisplayNames[{id}]: {message}"))?,
                _ => return Err(format!("modelDisplayNames[{id}]: must be a string or null")),
            };
            Ok((id.clone(), name))
        })
        .collect()
}

fn plan_label(value: &Value) -> Result<String, String> {
    let text = value.as_str().ok_or("must be a string or null")?;
    crate::profiles::plan_label::normalize_manual_plan_label(text)
}

fn boolean(value: &Value) -> Result<bool, String> {
    value.as_bool().ok_or_else(|| "must be a boolean or null".to_string())
}

fn positive_count(value: &Value) -> Result<u64, String> {
    value
        .as_u64()
        .filter(|count| *count > 0 && *count <= i64::MAX as u64)
        .ok_or_else(|| "must be a positive integer or null".to_string())
}

fn http_url(value: &Value) -> Result<String, String> {
    let text = value.as_str().ok_or("must be a string or null")?.trim();
    let url = url::Url::parse(text).map_err(|error| format!("invalid URL: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("must be an http(s) URL with a host".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("must not embed credentials".into());
    }
    Ok(text.to_string())
}

/// v2 keys; the legacy `chatgpt` is accepted as codex.
fn provider_category(value: &Value) -> Result<crate::profiles::provider_category::ProviderCategory, String> {
    value
        .as_str()
        .and_then(crate::profiles::provider_category::ProviderCategory::parse)
        .ok_or_else(|| "must be kimi, glm, zai, claude, deepseek, codex, other or null".to_string())
}

fn color(value: &Value) -> Result<String, String> {
    let text = value.as_str().ok_or("must be a string or null")?.trim();
    let valid = text.len() == 7
        && text.starts_with('#')
        && text[1..].bytes().all(|b| b.is_ascii_hexdigit());
    valid
        .then(|| text.to_ascii_uppercase())
        .ok_or_else(|| "must be #RRGGBB or null".to_string())
}

pub(super) fn parse_patch(body: &[u8]) -> Result<ProfileApiPatch, String> {
    let value: Value =
        serde_json::from_slice(body).map_err(|error| format!("JSON object body required: {error}"))?;
    let body = value.as_object().ok_or("JSON object body required")?;
    if let Some(unknown) = body.keys().find(|key| !PATCH_FIELDS.contains(&key.as_str())) {
        return Err(format!("unsupported field: {unknown}"));
    }
    let api_key = match body.get("apiKey") {
        None => None,
        Some(Value::String(key)) => {
            let key = key.trim();
            if key.is_empty()
                || key.len() > MAX_API_KEY_BYTES
                || key.chars().any(|c| c.is_control() || c.is_whitespace())
            {
                return Err(format!(
                    "apiKey: must be 1-{MAX_API_KEY_BYTES} non-whitespace characters"
                ));
            }
            Some(ApiKey(key.to_string()))
        }
        Some(_) => return Err("apiKey: must be a string (clearing is not supported)".into()),
    };
    Ok(ProfileApiPatch {
        codex_config: CodexProfileConfigPatch {
            model: tri_state(body, "model", visible_string(200))?,
            model_provider: tri_state(body, "modelProvider", token(64, "_.-"))?,
            forced_login_method: tri_state(body, "forcedLoginMethod", one_of(&["chatgpt", "api"]))?,
            model_reasoning_effort: tri_state(body, "modelReasoningEffort", token(32, "_-"))?,
            model_reasoning_summary: tri_state(
                body,
                "modelReasoningSummary",
                one_of(MODEL_REASONING_SUMMARY_VALUES),
            )?,
            model_context_window: tri_state(body, "modelContextWindow", positive_count)?,
            model_auto_compact_token_limit: tri_state(body, "modelAutoCompactTokenLimit", positive_count)?,
            provider_name: tri_state(body, "providerName", visible_string(100))?,
            provider_base_url: tri_state(body, "providerBaseUrl", http_url)?,
            provider_env_key: tri_state(body, "providerEnvKey", env_name)?,
            provider_wire_api: tri_state(body, "providerWireApi", one_of(&["responses"]))?,
            request_max_retries: tri_state(body, "requestMaxRetries", bounded_count(100))?,
            stream_max_retries: tri_state(body, "streamMaxRetries", bounded_count(100))?,
            stream_idle_timeout_ms: tri_state(body, "streamIdleTimeoutMs", bounded_count(86_400_000))?,
            websocket_connect_timeout_ms: tri_state(body, "websocketConnectTimeoutMs", bounded_count(600_000))?,
            requires_openai_auth: tri_state(body, "requiresOpenaiAuth", boolean)?,
            update_plan_enabled: tri_state(body, "updatePlanEnabled", boolean)?,
            web_search: tri_state(body, "webSearch", one_of(WEB_SEARCH_VALUES))?,
            tool_schema_compat: tri_state(body, "toolSchemaCompat", one_of(TOOL_SCHEMA_COMPAT_VALUES))?,
            supports_websockets: tri_state(body, "supportsWebsockets", boolean)?,
            provider_instructions_field: tri_state(body, "providerInstructionsField", boolean)?,
            ..CodexProfileConfigPatch::default()
        },
        color: tri_state(body, "color", color)?,
        provider_category: tri_state(body, "providerCategory", provider_category)?,
        allow_session_switch: tri_state(body, "allowSessionSwitch", boolean)?,
        native_subagents: tri_state(body, "nativeSubagents", boolean)?,
        delegation_wait: tri_state(body, "delegationWait", boolean)?,
        browser_tools: match body.get("browserTools") {
            None => None,
            Some(Value::Bool(value)) => Some(*value),
            Some(_) => return Err("browserTools: must be a boolean".into()),
        },
        search_tool: match body.get("searchTool") {
            None => None,
            Some(Value::Bool(value)) => Some(*value),
            Some(_) => return Err("searchTool: must be a boolean".into()),
        },
        private_chatgpt_auth_file: match body.get("privateChatgptAuthFile") {
            None => None,
            Some(Value::Bool(value)) => Some(*value),
            Some(_) => return Err("privateChatgptAuthFile: must be a boolean".into()),
        },
        agent_use_requires_approval: match body.get("agentUseRequiresApproval") {
            None => None,
            Some(Value::Bool(value)) => Some(*value),
            Some(_) => return Err("agentUseRequiresApproval: must be a boolean".into()),
        },
        agent_turn_questions: tri_state(body, "agentTurnQuestions", agent_turn_questions)?,
        claude_file_checkpointing: tri_state(body, "claudeFileCheckpointing", boolean)?,
        claude_allow_write: tri_state(body, "claudeAllowWrite", boolean)?,
        claude_allow_exec: tri_state(body, "claudeAllowExec", boolean)?,
        plan_label: tri_state(body, "planLabel", plan_label)?,
        claude_account: tri_state(body, "claudeAccount", claude_account_id)?,
        model_display_names: model_display_names(body)?,
        api_key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_profile_patch_is_boolean_and_default_off() {
        assert_eq!(parse_patch(br#"{"browserTools":true}"#).unwrap().browser_tools,Some(true));
        assert_eq!(parse_patch(br#"{"browserTools":false}"#).unwrap().browser_tools,Some(false));
        assert_eq!(parse_patch(br#"{}"#).unwrap().browser_tools,None);
        for bad in [br#"{"browserTools":null}"#.as_slice(),br#"{"browserTools":"true"}"#,br#"{"browserTools":1}"#] {
            assert!(parse_patch(bad).is_err());
        }
    }

    #[test]
    fn private_chatgpt_auth_file_patch_is_boolean() {
        assert_eq!(parse_patch(br#"{"privateChatgptAuthFile":true}"#).unwrap().private_chatgpt_auth_file, Some(true));
        assert_eq!(parse_patch(br#"{}"#).unwrap().private_chatgpt_auth_file, None);
        for bad in [br#"{"privateChatgptAuthFile":null}"#.as_slice(), br#"{"privateChatgptAuthFile":"on"}"#] {
            assert!(parse_patch(bad).unwrap_err().starts_with("privateChatgptAuthFile: "));
        }
    }

    #[test]
    fn search_tool_profile_patch_is_boolean() {
        assert_eq!(parse_patch(br#"{"searchTool":true}"#).unwrap().search_tool, Some(true));
        assert_eq!(parse_patch(br#"{"searchTool":false}"#).unwrap().search_tool, Some(false));
        assert_eq!(parse_patch(br#"{}"#).unwrap().search_tool, None);
        for bad in [br#"{"searchTool":null}"#.as_slice(), br#"{"searchTool":"on"}"#, br#"{"searchTool":1}"#] {
            assert!(parse_patch(bad).unwrap_err().starts_with("searchTool: "));
        }
    }

    #[test]
    fn item_paths_decode_and_reject_nested() {
        assert!(is_list_path("/v2/profiles"));
        assert_eq!(parse_item_path("/v2/profiles/GLM"), Some("GLM".into()));
        assert_eq!(parse_item_path("/v2/profiles/my%20glm"), Some("my glm".into()));
        assert_eq!(parse_item_path("/v2/profiles/GLM/quota"), None);
        assert_eq!(parse_item_path("/v2/profiles/a%2Fb"), None);
        assert_eq!(parse_item_path("/v2/profiles/"), None);
        assert_eq!(parse_duplicate_path("/v2/profiles/my%20glm/duplicate"), Some("my glm".into()));
        assert_eq!(parse_duplicate_path("/v2/profiles/duplicate"), None);
        assert_eq!(parse_duplicate_path("/v2/profiles/a/b/duplicate"), None);
        assert!(is_profiles_path("/v2/profiles/GLM/duplicate"));
        assert!(is_profiles_path(PRESETS_PATH));
        assert!(!is_profiles_path("/v2/profiles/GLM/quota"));
    }

    #[test]
    fn create_body_defaults_and_validation() {
        let legacy = parse_create(br#"{"name":" GLM "}"#).unwrap();
        assert_eq!(legacy.name, "GLM");
        assert_eq!(legacy.kind, NewProfileKind::ApiKey);
        assert!(legacy.preset.is_none() && legacy.api_key.is_none());

        let full = parse_create(
            br#"{"name":"kimi","kind":"apiKey","preset":"kimi","baseUrl":"https://api.moonshot.cn/v1",
                 "model":"kimi-k2","apiKey":"sk-test-only"}"#,
        )
        .unwrap();
        assert_eq!(full.preset.unwrap().id, "moonshot");
        assert_eq!(full.base_url.as_deref(), Some("https://api.moonshot.cn/v1"));
        assert_eq!(full.model.as_deref(), Some("kimi-k2"));
        assert!(!format!("{full:?}").contains("sk-test-only"));
        assert_eq!(parse_create(br#"{"name":"c","kind":"claude"}"#).unwrap().kind, NewProfileKind::Claude);
        assert_eq!(parse_create(br#"{"name":"w","kind":"chatgpt","model":"gpt-5.5"}"#).unwrap().kind, NewProfileKind::Chatgpt);
        for bad in [
            r#"{}"#,
            r#"{"name":""}"#,
            r#"{"name":"a/b"}"#,
            r#"{"name":"x","kind":"api-key"}"#,
            r#"{"name":"x","preset":"nope"}"#,
            r#"{"name":"x","baseUrl":"ftp://x"}"#,
            r#"{"name":"x","kind":"chatgpt","preset":"zai"}"#,
            r#"{"name":"x","kind":"claude","apiKey":"k"}"#,
            r#"{"name":"x","apiKey":"has space"}"#,
            r#"{"name":"x","extra":1}"#,
            r#"[]"#,
        ] {
            assert!(parse_create(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn name_bodies_for_duplicate_and_rename() {
        assert_eq!(parse_name_body(br#"{"name":"copy"}"#).unwrap(), "copy");
        assert!(parse_name_body(br#"{}"#).is_err());
        assert!(parse_name_body(br#"{"name":"copy","model":"x"}"#).is_err());
        assert_eq!(parse_name_field(br#"{"model":"x"}"#).unwrap(), None);
        assert_eq!(parse_name_field(br#"{"name":"new","model":"x"}"#).unwrap().as_deref(), Some("new"));
        assert!(parse_name_field(br#"{"name":1}"#).is_err());
        // PATCH accepts name next to config fields.
        let patch = parse_patch(br#"{"name":"new","model":"glm-5"}"#).unwrap();
        assert_eq!(patch.codex_config.model, ConfigValueUpdate::Set("glm-5".into()));
    }

    #[test]
    fn patch_is_tri_state_and_strict() {
        let patch = parse_patch(
            br##"{"model":" glm-5 ","modelReasoningEffort":null,"modelContextWindow":200000,
                 "color":"#aabbcc","allowSessionSwitch":false,"providerEnvKey":"GLM_KEY",
                 "requestMaxRetries":0,"supportsWebsockets":null,"forcedLoginMethod":"api",
                 "modelReasoningSummary":"concise"}"##,
        )
        .unwrap();
        assert_eq!(patch.codex_config.model, ConfigValueUpdate::Set("glm-5".into()));
        assert_eq!(patch.codex_config.model_reasoning_effort, ConfigValueUpdate::Clear);
        assert_eq!(patch.codex_config.model_reasoning_summary, ConfigValueUpdate::Set("concise".into()));
        assert_eq!(patch.codex_config.model_context_window, ConfigValueUpdate::Set(200000));
        assert_eq!(patch.codex_config.model_provider, ConfigValueUpdate::Unchanged);
        assert_eq!(patch.color, ConfigValueUpdate::Set("#AABBCC".into()));
        assert_eq!(patch.allow_session_switch, ConfigValueUpdate::Set(false));
        assert!(patch.api_key.is_none());
        assert_eq!(patch.codex_config.provider_env_key, ConfigValueUpdate::Set("GLM_KEY".into()));
        assert_eq!(patch.codex_config.request_max_retries, ConfigValueUpdate::Set(0));
        assert_eq!(patch.codex_config.supports_websockets, ConfigValueUpdate::Clear);
        assert_eq!(patch.codex_config.provider_instructions_field, ConfigValueUpdate::Unchanged);
        assert_eq!(
            parse_patch(br#"{"providerInstructionsField":true}"#).unwrap().codex_config.provider_instructions_field,
            ConfigValueUpdate::Set(true)
        );
        assert!(parse_patch(br#"{"providerInstructionsField":"yes"}"#).is_err());
        assert_eq!(patch.codex_config.forced_login_method, ConfigValueUpdate::Set("api".into()));
        assert_eq!(patch.provider_category, ConfigValueUpdate::Unchanged);
        use crate::profiles::provider_category::ProviderCategory;
        assert_eq!(
            parse_patch(br#"{"providerCategory":"zai"}"#).unwrap().provider_category,
            ConfigValueUpdate::Set(ProviderCategory::Zai)
        );
        assert_eq!(
            parse_patch(br#"{"providerCategory":"chatgpt"}"#).unwrap().provider_category,
            ConfigValueUpdate::Set(ProviderCategory::Codex)
        );
        assert_eq!(parse_patch(br#"{"providerCategory":null}"#).unwrap().provider_category, ConfigValueUpdate::Clear);

        for bad in [
            r#"{"providerCategory":"openai"}"#,
            r#"{"providerCategory":1}"#,
            r#"{"unknown":1}"#,
            r#"{"modelContextWindow":0}"#,
            r#"{"modelContextWindow":-5}"#,
            r#"{"contextWindow":1000}"#,
            r#"{"providerBaseUrl":"ftp://x"}"#,
            r#"{"providerBaseUrl":"https://user:pw@example.com"}"#,
            r#"{"providerEnvKey":"lower"}"#,
            r#"{"providerWireApi":"chat"}"#,
            r#"{"requestMaxRetries":101}"#,
            r#"{"forcedLoginMethod":"oauth"}"#,
            r#"{"color":"red"}"#,
            r#"{"modelProvider":"bad provider"}"#,
            r#"{"apiKey":null}"#,
            r#"{"apiKey":"has space"}"#,
            r#"{"allowSessionSwitch":"yes"}"#,
            r#"{"modelReasoningSummary":"verbose"}"#,
            r#"{"modelReasoningSummary":1}"#,
            r#"[]"#,
        ] {
            assert!(parse_patch(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn reasoning_summary_patch() {
        for value in ["auto", "concise", "detailed", "none"] {
            let body = format!(r#"{{"modelReasoningSummary":" {value} "}}"#);
            assert_eq!(
                parse_patch(body.as_bytes()).unwrap().codex_config.model_reasoning_summary,
                ConfigValueUpdate::Set(value.to_string())
            );
        }
        assert_eq!(
            parse_patch(br#"{"modelReasoningSummary":null}"#).unwrap().codex_config.model_reasoning_summary,
            ConfigValueUpdate::Clear
        );
        let unchanged = parse_patch(br#"{"modelReasoningEffort":"high"}"#).unwrap();
        assert_eq!(unchanged.codex_config.model_reasoning_summary, ConfigValueUpdate::Unchanged);
        for bad in [
            r#"{"modelReasoningSummary":"verbose"}"#,
            r#"{"modelReasoningSummary":"Auto"}"#,
            r#"{"modelReasoningSummary":""}"#,
            r#"{"modelReasoningSummary":1}"#,
            r#"{"modelReasoningSummary":true}"#,
            r#"{"modelReasoningSummary":["auto"]}"#,
        ] {
            let error = parse_patch(bad.as_bytes()).unwrap_err();
            assert!(error.starts_with("modelReasoningSummary: "), "{bad}: {error}");
        }
        assert_eq!(
            parse_patch(br#"{"reasoningSummary":"auto"}"#).unwrap_err(),
            "unsupported field: reasoningSummary"
        );
    }

    #[test]
    fn web_search_patch() {
        for value in ["disabled", "cached", "indexed", "live"] {
            let body = format!(r#"{{"webSearch":"{value}"}}"#);
            assert_eq!(
                parse_patch(body.as_bytes()).unwrap().codex_config.web_search,
                ConfigValueUpdate::Set(value.to_string())
            );
        }
        assert_eq!(
            parse_patch(br#"{"webSearch":null}"#).unwrap().codex_config.web_search,
            ConfigValueUpdate::Clear
        );
        let unchanged = parse_patch(br#"{"modelReasoningEffort":"high"}"#).unwrap();
        assert_eq!(unchanged.codex_config.web_search, ConfigValueUpdate::Unchanged);
        for bad in [r#"{"webSearch":"off"}"#, r#"{"webSearch":false}"#, r#"{"webSearch":""}"#] {
            let error = parse_patch(bad.as_bytes()).unwrap_err();
            assert!(error.starts_with("webSearch: "), "{bad}: {error}");
        }
    }

    #[test]
    fn tool_schema_compat_patch() {
        for value in ["off", "strict"] {
            let body = format!(r#"{{"toolSchemaCompat":"{value}"}}"#);
            assert_eq!(
                parse_patch(body.as_bytes()).unwrap().codex_config.tool_schema_compat,
                ConfigValueUpdate::Set(value.to_string())
            );
        }
        assert_eq!(
            parse_patch(br#"{"toolSchemaCompat":null}"#).unwrap().codex_config.tool_schema_compat,
            ConfigValueUpdate::Clear
        );
        let unchanged = parse_patch(br#"{"webSearch":"disabled"}"#).unwrap();
        assert_eq!(unchanged.codex_config.tool_schema_compat, ConfigValueUpdate::Unchanged);
        for bad in [r#"{"toolSchemaCompat":"gemini"}"#, r#"{"toolSchemaCompat":true}"#, r#"{"toolSchemaCompat":""}"#] {
            let error = parse_patch(bad.as_bytes()).unwrap_err();
            assert!(error.starts_with("toolSchemaCompat: "), "{bad}: {error}");
        }
    }

    #[test]
    fn update_plan_enabled_patch() {
        for value in [true, false] {
            let body = format!(r#"{{"updatePlanEnabled":{value}}}"#);
            assert_eq!(
                parse_patch(body.as_bytes()).unwrap().codex_config.update_plan_enabled,
                ConfigValueUpdate::Set(value)
            );
        }
        assert_eq!(
            parse_patch(br#"{"updatePlanEnabled":null}"#).unwrap().codex_config.update_plan_enabled,
            ConfigValueUpdate::Clear
        );
        let unchanged = parse_patch(br#"{"modelReasoningEffort":"high"}"#).unwrap();
        assert_eq!(unchanged.codex_config.update_plan_enabled, ConfigValueUpdate::Unchanged);
        for bad in [
            r#"{"updatePlanEnabled":"true"}"#,
            r#"{"updatePlanEnabled":1}"#,
            r#"{"updatePlanEnabled":{"enabled":true}}"#,
        ] {
            let error = parse_patch(bad.as_bytes()).unwrap_err();
            assert!(error.starts_with("updatePlanEnabled: "), "{bad}: {error}");
        }
    }

    #[test]
    fn claude_file_checkpointing_patch() {
        assert_eq!(parse_patch(br#"{}"#).unwrap().claude_file_checkpointing, ConfigValueUpdate::Unchanged);
        assert_eq!(parse_patch(br#"{"claudeFileCheckpointing":null}"#).unwrap().claude_file_checkpointing, ConfigValueUpdate::Clear);
        assert_eq!(parse_patch(br#"{"claudeFileCheckpointing":true}"#).unwrap().claude_file_checkpointing, ConfigValueUpdate::Set(true));
        assert_eq!(parse_patch(br#"{"claudeFileCheckpointing":false}"#).unwrap().claude_file_checkpointing, ConfigValueUpdate::Set(false));
        for bad in [r#"{"claudeFileCheckpointing":"on"}"#, r#"{"claudeFileCheckpointing":1}"#] {
            assert!(parse_patch(bad.as_bytes()).is_err());
        }
    }

    #[test]
    fn claude_tools_patch() {
        let patch = parse_patch(br#"{"claudeAllowWrite":true,"claudeAllowExec":false}"#).unwrap();
        assert_eq!((patch.claude_allow_write, patch.claude_allow_exec), (ConfigValueUpdate::Set(true), ConfigValueUpdate::Set(false)));
        assert_eq!(parse_patch(br#"{}"#).unwrap().claude_allow_write, ConfigValueUpdate::Unchanged);
        assert_eq!(parse_patch(br#"{"claudeAllowWrite":null}"#).unwrap().claude_allow_write, ConfigValueUpdate::Clear);
        for bad in [r#"{"claudeAllowExec":"on"}"#] {
            assert!(parse_patch(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn model_display_names_patch() {
        let parsed = parse_patch(br#"{"modelDisplayNames":{"gpt-6-astra":" Astra 6 ","Astra [400K]":null,"x":""}}"#)
            .unwrap()
            .model_display_names;
        assert_eq!(
            parsed,
            [
                ("Astra [400K]".to_string(), None),
                ("gpt-6-astra".to_string(), Some("Astra 6".to_string())),
                ("x".to_string(), None),
            ]
            .into()
        );
        assert!(parse_patch(br#"{}"#).unwrap().model_display_names.is_empty());
        let bidi = json!({"modelDisplayNames": {"a": format!("a{}b", char::from_u32(0x202e).unwrap())}}).to_string();
        for bad in [
            r#"{"modelDisplayNames":["a"]}"#,
            r#"{"modelDisplayNames":{"a":1}}"#,
            r#"{"modelDisplayNames":{" a":"x"}}"#,
            bidi.as_str(),
        ] {
            assert!(parse_patch(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn agent_turn_questions_patch() {
        assert_eq!(
            parse_patch(br#"{"agentTurnQuestions":"wait"}"#).unwrap().agent_turn_questions,
            ConfigValueUpdate::Set(AgentTurnQuestions::Wait)
        );
        assert_eq!(
            parse_patch(br#"{"agentTurnQuestions":"decide"}"#).unwrap().agent_turn_questions,
            ConfigValueUpdate::Set(AgentTurnQuestions::Decide)
        );
        assert_eq!(
            parse_patch(br#"{"agentTurnQuestions":null}"#).unwrap().agent_turn_questions,
            ConfigValueUpdate::Clear
        );
        assert_eq!(
            parse_patch(br#"{}"#).unwrap().agent_turn_questions,
            ConfigValueUpdate::Unchanged
        );
        for bad in [r#"{"agentTurnQuestions":"ask"}"#, r#"{"agentTurnQuestions":true}"#] {
            assert!(parse_patch(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn plan_label_patch() {
        assert_eq!(
            parse_patch(br#"{"planLabel":" GLM Pro "}"#).unwrap().plan_label,
            ConfigValueUpdate::Set("GLM Pro".into())
        );
        assert_eq!(parse_patch(br#"{"planLabel":null}"#).unwrap().plan_label, ConfigValueUpdate::Clear);
        assert_eq!(parse_patch(br#"{}"#).unwrap().plan_label, ConfigValueUpdate::Unchanged);
        let forty = format!(r#"{{"planLabel":"{}"}}"#, "套".repeat(40));
        assert!(parse_patch(forty.as_bytes()).is_ok());
        let long = format!(r#"{{"planLabel":"{}"}}"#, "a".repeat(41));
        for bad in [long.as_str(), r#"{"planLabel":""}"#, r#"{"planLabel":" "}"#, r#"{"planLabel":1}"#,
                    r#"{"planLabel":"a\nb"}"#] {
            assert!(parse_patch(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn api_key_patch_error_and_debug_never_echo_secret() {
        let patch = parse_patch(br#"{"apiKey":"sk-very-secret"}"#).unwrap();
        assert!(!format!("{patch:?}").contains("sk-very-secret"));
        let error = parse_patch(br#"{"apiKey":"sk-very secret"}"#).unwrap_err();
        assert!(!error.contains("sk-very"));
    }

    /// One request through `handle` on a real socket; `remote` = accepted on
    /// a non-loopback listener. Returns (status line + headers + body).
    fn call(method: &str, path: &str, body: &str, remote: bool) -> (u16, Value) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            use std::io::Read;
            let mut stream = std::net::TcpStream::connect(address).unwrap();
            let mut text = String::new();
            stream.read_to_string(&mut text).unwrap();
            text
        });
        let (mut stream, peer) = listener.accept().unwrap();
        crate::management::listeners::set_current_connection(Some(
            crate::management::listeners::ConnectionInfo { remote, peer },
        ));
        let request = SimpleHttpRequest {
            method: method.into(),
            path: path.into(),
            headers: Default::default(),
            body: body.as_bytes().to_vec(),
        };
        handle(&mut stream, &request, path).unwrap();
        crate::management::listeners::set_current_connection(None);
        drop(stream);
        let response = client.join().unwrap();
        let status = response[9..12].parse().unwrap();
        let json = response.split_once("\r\n\r\n").map(|(_, body)| body).unwrap_or_default();
        (status, serde_json::from_str(json).unwrap_or(Value::Null))
    }

    fn audit_text() -> String {
        let path = crate::config::paths::runtime_dir().unwrap().join(transfer::AUDIT_DIR).join("audit.jsonl");
        std::fs::read_to_string(path).unwrap_or_default()
    }

    #[test]
    fn transfer_routes_loopback_gate_round_trip_and_audit_without_values() {
        const KEY: &str = "route-test-placeholder-key";
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        let created = call(
            "POST",
            "/v2/profiles",
            &json!({ "name": "GLM", "kind": "apiKey", "preset": "glm", "apiKey": KEY }).to_string(),
            false,
        );
        assert_eq!(created.0, 201, "{created:?}");

        // Credentials over a remote (tailnet / LAN) listener: refused.
        let (status, body) = call("POST", "/v2/profiles/GLM/export", r#"{"includeCredentials":true}"#, true);
        assert_eq!(status, 403, "{body}");
        assert_eq!(body["error"]["code"], "credentials_loopback_only", "{body}");
        // Settings only over the same listener: allowed, no key.
        let (status, remote) = call("POST", "/v2/profiles/GLM/export", "", true);
        assert_eq!(status, 200, "{remote}");
        assert_eq!(remote["credentialsIncluded"], false);
        assert!(!remote.to_string().contains(KEY));
        // GET is not a thing (the bundle must not be cached or logged).
        assert_eq!(call("GET", "/v2/profiles/GLM/export", "", false).0, 405);

        // Loopback (this machine / SSH tunnel): the key travels.
        let (status, local) = call("POST", "/v2/profiles/GLM/export", r#"{"includeCredentials":true}"#, false);
        assert_eq!(status, 200, "{local}");
        assert_eq!(local["credentialsIncluded"], true);
        assert_eq!(local["bundle"]["credentials"]["apiKey"], KEY);

        // Import: clash refused, new name created with warnings.
        let import = |name: &str, bundle: &Value| {
            call("POST", IMPORT_PATH, &json!({ "bundle": bundle, "name": name }).to_string(), false)
        };
        let (status, clash) = import("GLM", &local["bundle"]);
        assert_eq!(status, 409, "{clash}");
        assert_eq!(clash["error"]["code"], "profile_name_conflict");
        assert!(!clash.to_string().contains(KEY));
        let (status, copied) = import("GLM (other host)", &local["bundle"]);
        assert_eq!(status, 201, "{copied}");
        assert_eq!((copied["imported"].as_bool(), copied["credentialsCopied"].as_bool()), (Some(true), Some(true)));
        assert_eq!(copied["warnings"], json!([]));
        assert_eq!(copied["profile"]["name"], "GLM (other host)");
        assert!(!copied.to_string().contains(KEY), "import response is secret-free");
        let (status, plain) = import("GLM 3", &remote["bundle"]);
        assert_eq!(status, 201);
        assert_eq!(plain["warnings"][0]["code"], "needsLogin");
        let mut newer = local["bundle"].clone();
        newer["schemaVersion"] = json!(9);
        let (status, refused) = import("GLM 4", &newer);
        assert_eq!((status, refused["error"]["code"].as_str()), (400, Some("unsupported_bundle")));
        assert!(!refused.to_string().contains(KEY));
        // A profile literally named "import" stays addressable by GET.
        assert_eq!(call("GET", IMPORT_PATH, "", false).0, 404);

        let audit = audit_text();
        assert!(!audit.contains(KEY), "audit holds no values: {audit}");
        assert_eq!(audit.matches("\"profileExport\"").count(), 2, "{audit}");
        assert_eq!(audit.matches("\"profileExportRefused\"").count(), 1, "{audit}");
        assert_eq!(audit.matches("\"profileImport\"").count(), 2, "{audit}");
        assert!(audit.contains("\"credentialsIncluded\":true"), "{audit}");
        // Nor the diagnostic error log.
        let errors = crate::config::paths::runtime_dir().unwrap().join("service-logs/management-errors.jsonl");
        assert!(!std::fs::read_to_string(errors).unwrap_or_default().contains(KEY));
    }

    #[test]
    fn transfer_bodies_are_strict() {
        assert!(!parse_export_body(b"").unwrap());
        assert!(!parse_export_body(b"{}").unwrap());
        assert!(parse_export_body(br#"{"includeCredentials":true}"#).unwrap());
        assert!(parse_export_body(br#"{"includeCredentials":"yes"}"#).is_err());
        assert!(parse_export_body(br#"{"other":1}"#).is_err());
        assert_eq!(parse_export_path("/v2/profiles/my%20glm/export"), Some("my glm".into()));
        assert!(is_profiles_path("/v2/profiles/GLM/export") && is_profiles_path(IMPORT_PATH));

        let (_, name) = parse_import_body(br#"{"bundle":{},"name":" new ","onConflict":"fail"}"#).unwrap();
        assert_eq!(name, "new");
        for bad in [
            r#"{"bundle":{}}"#,
            r#"{"name":"x"}"#,
            r#"{"bundle":[],"name":"x"}"#,
            r#"{"bundle":{},"name":"x","onConflict":"overwrite"}"#,
            r#"{"bundle":{},"name":"x","extra":1}"#,
        ] {
            assert!(parse_import_body(bad.as_bytes()).is_err(), "{bad}");
        }
        let error = parse_import_body(br#"{"bundle":{"credentials":{"apiKey":"sk-leak"}},"name":"x",}"#).unwrap_err();
        assert!(!error.contains("sk-leak"), "{error}");
    }
}
