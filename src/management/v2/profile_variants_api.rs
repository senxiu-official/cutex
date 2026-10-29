//! r265 runtime variant routes (capability `profile-runtime-variants`;
//! docs/development/r265-profile-variants.md §5). Root Management only.
//!
//! ```text
//! POST   /v2/profiles/{p}/variants        {runtime, id?, defaults?:{model?, reasoningEffort?}, baseUrl?} → 201 {variant, profile}
//! PATCH  /v2/profiles/{p}/variants/{v}    {defaults?:{model?|null, reasoningEffort?|null}, baseUrl?|null} → 200 {variant, profile}
//! DELETE /v2/profiles/{p}/variants/{v}                                                         → 200 {deleted, defaultVariant, profile}
//! ```
//!
//! `baseUrl` (profile-relay-runtimes, additive): a DSH variant's address,
//! stored as the profile's `interfaceBaseUrls.deepseekNative` (null = back
//! to the official default). Kimi Code variants use the profile's base URL.

use std::net::TcpStream;

use serde_json::{json, Map, Value};

use crate::config::global_settings::ConfigValueUpdate;
use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::profiles::runtime_variants::{self as variants, Refusal, VariantDefaults};

pub enum Route {
    Collection(String),
    Item(String, String),
}

pub fn parse_path(path: &str) -> Option<Route> {
    let rest = path.strip_prefix("/v2/profiles/")?;
    let (profile, tail) = rest.split_once("/variants")?;
    let profile = super::profile_quota::percent_decode(profile).filter(|p| !p.is_empty() && !p.contains('/') && !p.chars().any(char::is_control))?;
    match tail {
        "" => Some(Route::Collection(profile)),
        _ => {
            let id = tail.strip_prefix('/')?;
            (!id.is_empty() && !id.contains('/')).then(|| Route::Item(profile, id.to_string()))
        }
    }
}

fn refusal(stream: &mut TcpStream, refusal: Refusal) -> anyhow::Result<()> {
    let reason = match refusal.status {
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Internal Server Error",
    };
    super::server::write_v2_error(stream, refusal.status, reason, refusal.code, &refusal.message, false, refusal.details)
}

fn invalid(stream: &mut TcpStream, message: impl Into<String>) -> anyhow::Result<()> {
    refusal(stream, Refusal { status: 400, code: "invalid_request", message: message.into(), details: json!({}) })
}

fn object(body: &[u8], allowed: &[&str]) -> Result<Map<String, Value>, String> {
    let value: Value = serde_json::from_slice(body).map_err(|error| format!("JSON object body required: {error}"))?;
    let Value::Object(map) = value else { return Err("JSON object body required".into()) };
    if let Some(unknown) = map.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(format!("unsupported field: {unknown}"));
    }
    Ok(map)
}

fn text(value: &Value, field: &str, max: usize) -> Result<String, String> {
    let text = value.as_str().ok_or_else(|| format!("{field}: must be a string"))?.trim();
    if text.is_empty() || text.len() > max || text.chars().any(char::is_control) {
        return Err(format!("{field}: 1-{max} visible characters"));
    }
    Ok(text.to_string())
}

/// `defaults` as tri-state updates (absent / null / value).
fn defaults_patch(value: Option<&Value>) -> Result<(ConfigValueUpdate<String>, ConfigValueUpdate<String>), String> {
    let Some(value) = value else { return Ok((ConfigValueUpdate::Unchanged, ConfigValueUpdate::Unchanged)) };
    let map = value.as_object().ok_or("defaults: must be an object")?;
    if let Some(unknown) = map.keys().find(|key| !["model", "reasoningEffort"].contains(&key.as_str())) {
        return Err(format!("defaults: unsupported field: {unknown}"));
    }
    let field = |key: &str, max| -> Result<ConfigValueUpdate<String>, String> {
        Ok(match map.get(key) {
            None => ConfigValueUpdate::Unchanged,
            Some(Value::Null) => ConfigValueUpdate::Clear,
            Some(value) => ConfigValueUpdate::Set(text(value, &format!("defaults.{key}"), max)?),
        })
    };
    Ok((field("model", 200)?, field("reasoningEffort", 32)?))
}

/// `baseUrl` of a variant write: absent / null / a plain http(s) URL.
fn base_url_value(value: Option<&Value>) -> Result<ConfigValueUpdate<String>, String> {
    match value {
        None => Ok(ConfigValueUpdate::Unchanged),
        Some(Value::Null) => Ok(ConfigValueUpdate::Clear),
        Some(Value::String(text)) => crate::profiles::interface_urls::normalize_base_url(text)
            .map(ConfigValueUpdate::Set)
            .map_err(|message| format!("baseUrl: {message}")),
        Some(_) => Err("baseUrl: must be a string or null".into()),
    }
}

fn variant_json(account: &crate::profiles::model::StoredAccount, profile_item: &Value, id: &str) -> Value {
    profile_item["runtimeVariants"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["id"] == id).cloned())
        .unwrap_or_else(|| json!({ "id": id, "profile": account.id }))
}

fn detail(profile: &str) -> anyhow::Result<Option<(crate::profiles::model::StoredAccount, Value)>> {
    let store = crate::profiles::settings_api::load_store()?;
    Ok(crate::profiles::settings_api::find(&store, profile).map(|account| (account.clone(), super::profiles_api::detail_item(account))))
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest, route: Route) -> anyhow::Result<()> {
    let _guard = crate::profiles::lifecycle::lock();
    match (request.method.as_str(), route) {
        ("POST", Route::Collection(profile)) => {
            let body = match object(&request.body, &["runtime", "id", "defaults", "baseUrl"]) {
                Ok(body) => body,
                Err(message) => return invalid(stream, message),
            };
            let Some(runtime) = body.get("runtime").and_then(Value::as_str) else {
                return invalid(stream, "runtime: required (codex, claude or dsh)");
            };
            let id = match body.get("id") {
                None | Some(Value::Null) => None,
                Some(Value::String(id)) => Some(id.as_str()),
                Some(_) => return invalid(stream, "id: must be a string"),
            };
            let defaults = match defaults_patch(body.get("defaults")) {
                Ok((model, effort)) => VariantDefaults {
                    model: if let ConfigValueUpdate::Set(model) = model { Some(model) } else { None },
                    reasoning_effort: if let ConfigValueUpdate::Set(effort) = effort { Some(effort) } else { None },
                },
                Err(message) => return invalid(stream, message),
            };
            let base_url = match base_url_value(body.get("baseUrl")) {
                Ok(ConfigValueUpdate::Set(url)) => Some(url),
                Ok(_) => None,
                Err(message) => return invalid(stream, message),
            };
            match variants::add_variant_with_address(&profile, runtime, id, defaults, base_url) {
                Ok(account) => {
                    let item = super::profiles_api::detail_item(&account);
                    let id = id.unwrap_or(runtime);
                    write_json_response(stream, 201, "Created", &json!({
                        "contractVersion": 2, "variant": variant_json(&account, &item, id), "profile": item,
                        "appliesTo": "newAgents",
                    }))
                }
                Err(error) => refusal(stream, error),
            }
        }
        ("PATCH", Route::Item(profile, id)) => {
            let body = match object(&request.body, &["defaults", "baseUrl"]) {
                Ok(body) => body,
                Err(message) => return invalid(stream, message),
            };
            let (model, effort) = match defaults_patch(body.get("defaults")) {
                Ok(patch) => patch,
                Err(message) => return invalid(stream, message),
            };
            let base_url = match base_url_value(body.get("baseUrl")) {
                Ok(update) => update,
                Err(message) => return invalid(stream, message),
            };
            let account = match detail(&profile) {
                Ok(Some((account, _))) => account,
                Ok(None) => return refusal(stream, Refusal { status: 404, code: "profile_not_found", message: format!("no profile {profile}"), details: json!({ "profile": profile }) }),
                Err(error) => return refusal(stream, Refusal { status: 500, code: "profile_store_failed", message: format!("{error:#}"), details: json!({}) }),
            };
            // The address first (its own validation); then the defaults.
            if !matches!(base_url, ConfigValueUpdate::Unchanged) {
                let interface = variants::variants(&account)
                    .into_iter()
                    .find(|variant| variant.id == id)
                    .and_then(|variant| variants::launch_interface(&account, &variant.runtime))
                    .filter(|interface| crate::profiles::interface_urls::stored_kinds_for(&account).contains(interface));
                let Some(interface) = interface else {
                    return invalid(stream, "baseUrl: only a DSH variant has its own address; other runtimes use the profile's base URL");
                };
                let mut patch = crate::profiles::settings_api::ProfileApiPatch::default();
                let url = match &base_url { ConfigValueUpdate::Set(url) => Some(url.clone()), _ => None };
                patch.interface_base_urls.insert(interface.as_str().to_string(), url);
                if let Err(error) = crate::profiles::settings_api::update_profile_locked(&account.id, &patch) {
                    return invalid(stream, format!("baseUrl: {error:#}"));
                }
            }
            let unchanged_defaults = matches!(model, ConfigValueUpdate::Unchanged) && matches!(effort, ConfigValueUpdate::Unchanged);
            let updated = if unchanged_defaults && !matches!(base_url, ConfigValueUpdate::Unchanged) {
                match detail(&account.id) {
                    Ok(Some((account, _))) => Ok(account),
                    _ => Err(Refusal { status: 500, code: "profile_store_failed", message: "profile store reread failed".into(), details: json!({}) }),
                }
            } else if id == variants::primary_id(&account) {
                // The primary's defaults are the profile's own model settings.
                let mut patch = crate::profiles::settings_api::ProfileApiPatch::default();
                patch.codex_config.model = model;
                patch.codex_config.model_reasoning_effort = effort;
                crate::profiles::settings_api::update_profile_locked(&account.id, &patch)
                    .map(|(account, _)| account)
                    .map_err(|error| Refusal { status: 400, code: "invalid_request", message: format!("{error:#}"), details: json!({}) })
            } else {
                variants::patch_variant(&account.id, &id, model, effort)
            };
            match updated {
                Ok(account) => {
                    let item = super::profiles_api::detail_item(&account);
                    write_json_response(stream, 200, "OK", &json!({
                        "contractVersion": 2, "variant": variant_json(&account, &item, &id), "profile": item,
                        "appliesTo": "newAgents",
                    }))
                }
                Err(error) => refusal(stream, error),
            }
        }
        ("DELETE", Route::Item(profile, id)) => match variants::delete_variant(&profile, &id) {
            Ok(account) => {
                let item = super::profiles_api::detail_item(&account);
                write_json_response(stream, 200, "OK", &json!({
                    "contractVersion": 2, "deleted": { "id": id }, "defaultVariant": variants::default_variant_id(&account), "profile": item,
                }))
            }
            Err(error) => refusal(stream, error),
        },
        _ => super::server::write_v2_error(stream, 405, "Method Not Allowed", "method_not_allowed",
            "use POST /v2/profiles/{p}/variants or PATCH|DELETE /v2/profiles/{p}/variants/{id}", false, json!({})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variant_paths_parse_exactly() {
        assert!(matches!(parse_path("/v2/profiles/DS/variants"), Some(Route::Collection(p)) if p == "DS"));
        assert!(matches!(parse_path("/v2/profiles/My%20DS/variants/dsh"), Some(Route::Item(p, v)) if p == "My DS" && v == "dsh"));
        assert!(parse_path("/v2/profiles/DS/variants/dsh/x").is_none());
        assert!(parse_path("/v2/profiles/DS/variantsx").is_none());
        assert!(parse_path("/v2/profiles//variants").is_none());
        assert!(parse_path("/v2/profiles/DS").is_none());
    }
}
