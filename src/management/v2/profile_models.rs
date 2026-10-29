//! Root-only profile model catalog routes:
//! `GET /v2/profiles/{profile}/models`,
//! `POST /v2/profiles/{profile}/models/discover` `{apply?: bool}`,
//! `PATCH|DELETE /v2/profiles/{profile}/models/{modelId}`,
//! `POST /v2/profiles/{profile}/models/{modelId}/reset`.
//! Path segments are percent-encoded; a model ID may contain `/` as `%2F`.
//!
//! `GET` always lists `models` (id, displayName, supportedReasoningEfforts,
//! defaultReasoningEffort, inputModalities, context limits, isDefault).
//! `catalogSupported` says whether the list is editable; official Codex routes
//! require `modelVariantsSupported` from the actual native server. `requestModel`
//! selects an immutable base-model binding, while `id` remains the local model
//! selection. `discoverySupported` is restricted to API-key custom providers.
//! `modelsSource.kind` says
//! where the list came from: `catalog`, `nativeCache` (the shared native
//! home's `models_cache.json`, with `fetchedAt` / `clientVersion`),
//! `nativeBundled` (the selected native CLI's compiled-in catalog) or
//! `claudeReference` (the selected Claude adapter's reference catalog) or
//! `claudeAliases` (its fallback); `null` with `modelsUnavailableReason` when
//! no source is available. Feature `profileModelsNative`; see
//! `profiles::native_models`. Claude rows carry the adapter's nominal context
//! window (`effective.contextWindowSource: "reference"`, null for aliases) and
//! no auto-compact limit (`autoCompactTokenLimitSource: "notApplicable"`);
//! `autoCompactApplies` is false for Claude profiles (absent on older cutex).
//! An official profile with edits reports `overlay` (the live upstream list
//! in `modelsSource.upstream` plus this profile's overlay) and
//! `unavailableModels`; feature `profileCatalogOverlay`, see
//! `profiles::catalog_overlay`.

use std::net::TcpStream;

use serde_json::{json, Map, Value};

use crate::http::server::write_json_response;
use crate::http::server::SimpleHttpRequest;
use crate::profiles::catalog_api::{self, ModelPatch, UNSUPPORTED_MODEL_FIELDS};
use crate::profiles::settings_api;

use super::profiles_api::{tri_state, visible_string};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelsRoute {
    List,
    Discover,
    Model(String),
    Reset(String),
}

pub fn parse_path(path: &str) -> Option<(String, ModelsRoute)> {
    let rest = path.strip_prefix("/v2/profiles/")?;
    let (profile, rest) = rest.split_once("/models")?;
    let decode = |segment: &str| {
        (!segment.is_empty())
            .then(|| super::profile_quota::percent_decode(segment))
            .flatten()
            .filter(|value| !value.is_empty() && !value.chars().any(char::is_control))
    };
    let profile = decode(profile).filter(|p| !p.contains('/'))?;
    let route = match rest {
        "" => ModelsRoute::List,
        "/discover" => ModelsRoute::Discover,
        _ => {
            let rest = rest.strip_prefix('/')?;
            match rest.split('/').collect::<Vec<_>>().as_slice() {
                [model] => ModelsRoute::Model(decode(model)?),
                [model, "reset"] => ModelsRoute::Reset(decode(model)?),
                _ => return None,
            }
        }
    };
    Some((profile, route))
}

fn error(stream: &mut TcpStream, status: u16, code: &str, message: &str) -> anyhow::Result<()> {
    let reason = match status {
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        502 => "Bad Gateway",
        _ => "Service Unavailable",
    };
    super::server::write_v2_error(stream, status, reason, code, message, status >= 500, json!({}))
}

fn count(max: u64) -> impl Fn(&Value) -> Result<u64, String> {
    move |value| {
        value
            .as_u64()
            .filter(|n| *n >= 1 && *n <= max)
            .ok_or_else(|| format!("must be an integer 1-{max} or null"))
    }
}

fn string_list(value: &Value, allowed: Option<&[&str]>) -> Result<Vec<String>, String> {
    let items = value.as_array().ok_or("must be an array of strings or null")?;
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let text = item.as_str().ok_or("must contain only strings")?.trim().to_string();
        let valid = !text.is_empty()
            && text.len() <= 32
            && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            && allowed.is_none_or(|allowed| allowed.contains(&text.as_str()));
        if !valid {
            return Err(match allowed {
                Some(allowed) => format!("values must be among {}", allowed.join(", ")),
                None => "values must be 1-32 characters of [A-Za-z0-9_-]".to_string(),
            });
        }
        if out.contains(&text) {
            return Err(format!("duplicate value {text}"));
        }
        out.push(text);
    }
    Ok(out)
}

const MODEL_FIELDS: &[&str] = &[
    "requestModel",
    "displayName",
    "contextWindow",
    "maxContextWindow",
    "autoCompactTokenLimit",
    "effectiveContextWindowPercent",
    "inputModalities",
    "supportedReasoningEfforts",
    "defaultReasoningEffort",
];

pub(super) fn parse_model_patch(body: &[u8]) -> Result<ModelPatch, String> {
    let value: Value = if body.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(body).map_err(|e| format!("JSON object body required: {e}"))?
    };
    let body: &Map<String, Value> = value.as_object().ok_or("JSON object body required")?;
    for key in body.keys() {
        if UNSUPPORTED_MODEL_FIELDS.contains(&key.as_str()) {
            return Err(format!("{key} is not supported by the cute-codex model catalog"));
        }
        if !MODEL_FIELDS.contains(&key.as_str()) {
            return Err(format!("unsupported field: {key}"));
        }
    }
    let tokens = 100_000_000;
    Ok(ModelPatch {
        request_model: tri_state(body, "requestModel", |v| {
            let id = visible_string(catalog_api::MAX_MODEL_ID_CHARS)(v)?;
            catalog_api::validate_model_id(&id).map_err(|e| e.to_string())?;
            Ok(id)
        })?,
        display_name: tri_state(body, "displayName", visible_string(200))?,
        context_window: tri_state(body, "contextWindow", count(tokens))?,
        max_context_window: tri_state(body, "maxContextWindow", count(tokens))?,
        auto_compact_token_limit: tri_state(body, "autoCompactTokenLimit", count(tokens))?,
        effective_context_window_percent: tri_state(body, "effectiveContextWindowPercent", count(100))?,
        input_modalities: tri_state(body, "inputModalities", |v| {
            let list = string_list(v, Some(&["text", "image"]))?;
            if !list.iter().any(|m| m == "text") {
                return Err("must include text".into());
            }
            Ok(list)
        })?,
        supported_reasoning_efforts: tri_state(body, "supportedReasoningEfforts", |v| string_list(v, None))?,
        default_reasoning_effort: tri_state(body, "defaultReasoningEffort", |v| {
            string_list(&json!([v]), None).map(|mut list| list.remove(0))
        })?,
    })
}

pub(super) fn handle(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    profile: &str,
    route: ModelsRoute,
) -> anyhow::Result<()> {
    let store = match settings_api::load_store() {
        Ok(store) => store,
        Err(e) => return error(stream, 503, "profile_store_unavailable", &format!("{e:#}")),
    };
    let Some(account) = settings_api::find(&store, profile).cloned() else {
        return error(stream, 404, "profile_not_found", "no profile has this ID or name");
    };
    let respond = |stream: &mut TcpStream, extra: Value| -> anyhow::Result<()> {
        let account = settings_api::load_store()
            .ok()
            .and_then(|store| settings_api::find(&store, &account.id).cloned())
            .unwrap_or_else(|| account.clone());
        match catalog_api::models_resource(&account) {
            Ok(mut view) => {
                if let (Some(view), Some(extra)) = (view.as_object_mut(), extra.as_object()) {
                    view.extend(extra.clone());
                }
                write_json_response(stream, 200, "OK", &view)
            }
            Err(e) => error(stream, 503, "catalog_unavailable", &format!("{e:#}")),
        }
    };
    match (request.method.as_str(), route) {
        ("GET", ModelsRoute::List) => respond(stream, json!({})),
        ("POST", ModelsRoute::Discover) => {
            let apply = match serde_json::from_slice::<Value>(if request.body.is_empty() { b"{}" } else { &request.body }) {
                Ok(Value::Object(body)) if body.keys().all(|k| k == "apply") => match body.get("apply") {
                    None => true,
                    Some(Value::Bool(apply)) => *apply,
                    Some(_) => return error(stream, 400, "invalid_request", "apply must be a boolean"),
                },
                _ => return error(stream, 400, "invalid_request", "body must be {apply?: boolean}"),
            };
            if !catalog_api::discovery_supported(&account) {
                return error(stream, 400, "catalog_unsupported", "this profile does not use a custom model catalog");
            }
            match catalog_api::discover(&account, apply) {
                // `refreshed`: provider-owned fields of existing rows that the
                // rediscovery corrected ({model, field, from, to}).
                Ok(refreshed) => respond(stream, json!({ "applied": apply, "refreshed": refreshed })),
                // Discovery errors are already secret-free (see model_discovery::fetch).
                Err(e) => error(stream, 502, "model_discovery_failed", &format!("{e:#}")),
            }
        }
        ("PATCH", ModelsRoute::Model(model)) => {
            let patch = match parse_model_patch(&request.body) {
                Ok(patch) => patch,
                Err(message) => return error(stream, 400, "invalid_request", &message),
            };
            match catalog_api::update_model(&account, &model, &patch) {
                Ok(created) => respond(stream, json!({ "changedModel": model, "created": created })),
                Err(e) => error(stream, 400, "invalid_request", &format!("{e:#}")),
            }
        }
        ("DELETE", ModelsRoute::Model(model)) => match catalog_api::remove_model(&account, &model) {
            Ok(()) => respond(stream, json!({ "removedModel": model })),
            Err(e) => error(stream, 400, "invalid_request", &format!("{e:#}")),
        },
        ("POST", ModelsRoute::Reset(model)) => match catalog_api::reset_model(&account, &model) {
            Ok(()) => respond(stream, json!({ "changedModel": model, "reset": true })),
            Err(e) => error(stream, 400, "invalid_request", &format!("{e:#}")),
        },
        _ => error(stream, 405, "method_not_allowed", "unsupported method for this models route"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::global_settings::ConfigValueUpdate;

    #[test]
    fn model_paths_decode_ids_with_slashes() {
        assert_eq!(parse_path("/v2/profiles/GLM/models"), Some(("GLM".into(), ModelsRoute::List)));
        assert_eq!(parse_path("/v2/profiles/GLM/models/discover"), Some(("GLM".into(), ModelsRoute::Discover)));
        assert_eq!(
            parse_path("/v2/profiles/GLM/models/zai%2Fglm-5"),
            Some(("GLM".into(), ModelsRoute::Model("zai/glm-5".into())))
        );
        assert_eq!(
            parse_path("/v2/profiles/GLM/models/glm-5.3/reset"),
            Some(("GLM".into(), ModelsRoute::Reset("glm-5.3".into())))
        );
        assert_eq!(parse_path("/v2/profiles/GLM/models/a/b/c"), None);
        assert_eq!(parse_path("/v2/profiles/GLM/modelsx"), None);
        assert_eq!(parse_path("/v2/profiles//models"), None);
        assert_eq!(parse_path("/v2/profiles/GLM/quota"), None);
    }

    #[test]
    fn model_patch_is_strict_and_reports_unsupported_fields() {
        let patch = parse_model_patch(
            br#"{"contextWindow":200000,"autoCompactTokenLimit":null,"inputModalities":["text","image"],
                 "supportedReasoningEfforts":["low","high"],"defaultReasoningEffort":"high"}"#,
        )
        .unwrap();
        assert_eq!(patch.context_window, ConfigValueUpdate::Set(200000));
        assert_eq!(patch.auto_compact_token_limit, ConfigValueUpdate::Clear);
        assert_eq!(patch.default_reasoning_effort, ConfigValueUpdate::Set("high".into()));
        let unsupported = parse_model_patch(br#"{"maxOutputTokens":8192}"#).unwrap_err();
        assert!(unsupported.contains("not supported"));
        for bad in [
            r#"{"requestModel":""}"#,
            r#"{"requestModel":123}"#,
            r#"{"requestModel":"bad\nmodel"}"#,
            r#"{"contextWindow":0}"#,
            r#"{"inputModalities":["image"]}"#,
            r#"{"inputModalities":["text","audio"]}"#,
            r#"{"supportedReasoningEfforts":["low","low"]}"#,
            r#"{"effectiveContextWindowPercent":101}"#,
            r#"{"other":1}"#,
        ] {
            assert!(parse_model_patch(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn variant_patch_and_named_paths_preserve_wire_identity() {
        let patch = parse_model_patch(br#"{"requestModel":"gpt-6-astra","displayName":"Astra [400K]"}"#).unwrap();
        assert_eq!(patch.request_model, ConfigValueUpdate::Set("gpt-6-astra".into()));
        assert_eq!(parse_model_patch(b"{}").unwrap().request_model, ConfigValueUpdate::Unchanged);
        assert_eq!(parse_model_patch(br#"{"requestModel":null}"#).unwrap().request_model, ConfigValueUpdate::Clear);
        assert_eq!(parse_path("/v2/profiles/p/models/Astra%20%5B400K%5D"),
            Some(("p".into(), ModelsRoute::Model("Astra [400K]".into()))));
    }
}
