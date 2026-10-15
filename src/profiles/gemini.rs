//! Gemini through third-party OpenAI-Responses-compatible API-key relays.
//!
//! Two pieces of knowledge a relay's `/models` does not expose:
//!
//! 1. Profile config: Gemini refuses a request that mixes Codex's built-in
//!    `web_search` tool with function tools (400 "Please enable
//!    tool_config.include_server_side_tool_invocations to use Built-in tools
//!    with Function calling."), so a Gemini profile sets the native root key
//!    `web_search = "disabled"` (profile-scoped; `PATCH /v2/profiles/{p}
//!    {"webSearch": null}` reverts it).
//! 2. Model metadata for discovered `gemini-*` catalog rows, taken from
//!    Google's Gemini API documentation (fetched 2026-10-14):
//!    - model pages <https://ai.google.dev/gemini-api/docs/models> and
//!      `…/docs/models/<model code>` (input token limit, input types),
//!    - thinking levels + defaults <https://ai.google.dev/gemini-api/docs/thinking>,
//!    - OpenAI `reasoning_effort` mapping
//!      <https://ai.google.dev/gemini-api/docs/openai>.
//!
//!    Relay-specific names absent from those pages (e.g.
//!    `gemini-2.5-flash-thinking`, `gemini-3.8-flash-tiered`) get no metadata.
//!    The overlay only fills unset fields, so owner edits made through the
//!    catalog API survive later discovery and launches.

use serde_json::{json, Value};
use toml::value::Table;

pub const GEMINI_PROVIDER_ID: &str = "gemini";
pub const GEMINI_PROVIDER_NAME: &str = "Gemini";
/// The preset's default model: the relay-recommended tiered Gemini 3.8 Flash
/// (owner + relay, 2026-10-14: runs tools correctly with web_search
/// disabled). A relay-specific name, so it gets no documented metadata; the
/// overlay only lists it first in pickers. Known-good alternative:
/// `gemini-3-flash`.
pub const GEMINI_DEFAULT_MODEL: &str = "gemini-3.8-flash-tiered";
/// Picker `priority` (ascending) for the recommended row; discovered rows
/// start at 0.
const RECOMMENDED_PRIORITY: i64 = -1;
/// Native `WebSearchMode` written by the preset.
pub const GEMINI_WEB_SEARCH: &str = "disabled";
/// Native `ToolSchemaCompat` written by the preset (`preset_spec`): Google
/// function declarations refuse mixed, typeless or nullable enums ("Google
/// does not support this JSON Schema enum"), which MCP tool schemas often
/// carry.
pub const GEMINI_TOOL_SCHEMA_COMPAT: &str = "strict";

pub fn is_gemini_provider(provider: &str) -> bool {
    provider.trim().eq_ignore_ascii_case(GEMINI_PROVIDER_ID)
}

/// The preset's extra profile config on top of the generic API-key layout.
pub fn apply_profile_preset(root: &mut Table) {
    root.insert("web_search".to_string(), toml::Value::String(GEMINI_WEB_SEARCH.to_string()));
}

/// Documented metadata for one Gemini model code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GeminiModel {
    pub slug: &'static str,
    /// Other names relays serve the same documented model under.
    pub aliases: &'static [&'static str],
    /// Documented input token limit.
    pub context_window: i64,
    /// Documented thinking levels (Codex reasoning efforts of the same name).
    pub efforts: &'static [&'static str],
    /// Documented default thinking level; `None` when the docs give none
    /// (or "On" / "Off" without a level): the request then omits the effort
    /// and Gemini uses the model's own default.
    pub default_effort: Option<&'static str>,
}

const MINIMAL_TO_HIGH: &[&str] = &["minimal", "low", "medium", "high"];
const LOW_TO_HIGH: &[&str] = &["low", "medium", "high"];
/// Every model below documents 1,048,576 input tokens and text, image, video,
/// audio (and PDF) input.
const ONE_M: i64 = 1_048_576;

pub const GEMINI_MODELS: &[GeminiModel] = &[
    // docs/models/gemini-4-argon; thinking table: Medium, "low, medium, high".
    GeminiModel { slug: "gemini-4-argon", aliases: &[], context_window: ONE_M, efforts: LOW_TO_HIGH, default_effort: Some("medium") },
    // docs/models/gemini-3.8-flash ("minimal is not supported"); thinking table: Medium.
    GeminiModel { slug: "gemini-3.8-flash", aliases: &[], context_window: ONE_M, efforts: LOW_TO_HIGH, default_effort: Some("medium") },
    // docs/models/gemini-3.6-flash; thinking table: Medium, minimal–high.
    GeminiModel { slug: "gemini-3.6-flash", aliases: &[], context_window: ONE_M, efforts: MINIMAL_TO_HIGH, default_effort: Some("medium") },
    // docs/models/gemini-3.5-flash-lite; thinking table: Minimal, minimal–high.
    GeminiModel { slug: "gemini-3.5-flash-lite", aliases: &[], context_window: ONE_M, efforts: MINIMAL_TO_HIGH, default_effort: Some("minimal") },
    // docs/models/gemini-3.1-flash-lite; levels from the OpenAI reasoning_effort
    // mapping (minimal/low/medium/high); no documented default.
    GeminiModel { slug: "gemini-3.1-flash-lite", aliases: &[], context_window: ONE_M, efforts: MINIMAL_TO_HIGH, default_effort: None },
    // docs/models/gemini-3-flash-preview; thinking table: High, minimal–high.
    // Relays list it without the `-preview` suffix.
    GeminiModel { slug: "gemini-3-flash-preview", aliases: &["gemini-3-flash"], context_window: ONE_M, efforts: MINIMAL_TO_HIGH, default_effort: Some("high") },
    // docs/models/gemini-2.5-pro; thinking table: On, "low, medium, high".
    GeminiModel { slug: "gemini-2.5-pro", aliases: &[], context_window: ONE_M, efforts: LOW_TO_HIGH, default_effort: None },
    // docs/models/gemini-2.5-flash; thinking table: On (medium), "low, medium, high".
    GeminiModel { slug: "gemini-2.5-flash", aliases: &[], context_window: ONE_M, efforts: LOW_TO_HIGH, default_effort: Some("medium") },
    // docs/models/gemini-2.5-flash-lite; thinking table: Off, "low, medium, high".
    GeminiModel { slug: "gemini-2.5-flash-lite", aliases: &[], context_window: ONE_M, efforts: LOW_TO_HIGH, default_effort: None },
];

pub fn known_model(slug: &str) -> Option<&'static GeminiModel> {
    let slug = slug.trim().to_ascii_lowercase();
    GEMINI_MODELS
        .iter()
        .find(|model| model.slug == slug || model.aliases.contains(&slug.as_str()))
}

/// Gemini image-generation models ("Nano Banana", `gemini-*-image*`): not
/// for agents.
pub fn is_image_generation_model(slug: &str) -> bool {
    let slug = slug.trim().to_ascii_lowercase();
    slug.starts_with("gemini-") && slug.contains("-image")
}

fn effort_description(effort: &str) -> &'static str {
    match effort {
        "minimal" => "Gemini thinking level minimal",
        "low" => "Gemini thinking level low",
        "medium" => "Gemini thinking level medium",
        _ => "Gemini thinking level high",
    }
}

fn unset(row: &Value, key: &str) -> bool {
    match row.get(key) {
        None | Some(Value::Null) => true,
        Some(Value::Array(items)) => items.is_empty(),
        _ => false,
    }
}

/// Fill documented metadata into an unset field of a `gemini-*` row. An
/// image-generation row is hidden from pickers once, when the overlay first
/// sees it (no `input_modalities` yet); listing it again afterwards sticks.
pub fn apply_model_overlay(row: &mut Value) -> bool {
    let Some(slug) = row.get("slug").and_then(Value::as_str).map(str::to_string) else {
        return false;
    };
    if !row.is_object() {
        return false;
    }
    let mut changed = false;
    // The recommended model sorts first once; a priority the owner set (any
    // value other than the discovery template's 0) is kept.
    if slug == GEMINI_DEFAULT_MODEL {
        if matches!(row.get("priority").and_then(Value::as_i64), None | Some(0)) {
            row["priority"] = json!(RECOMMENDED_PRIORITY);
            return true;
        }
        return false;
    }
    if is_image_generation_model(&slug) {
        if row.get("input_modalities").is_none() {
            row["input_modalities"] = json!(["text", "image"]);
            row["visibility"] = json!("hide");
            changed = true;
        }
        return changed;
    }
    let Some(model) = known_model(&slug) else {
        return false;
    };
    if row.get("input_modalities").is_none() {
        row["input_modalities"] = json!(["text", "image"]);
        changed = true;
    }
    if unset(row, "context_window") {
        row["context_window"] = json!(model.context_window);
        changed = true;
    }
    if unset(row, "supported_reasoning_levels") {
        row["supported_reasoning_levels"] = Value::Array(
            model
                .efforts
                .iter()
                .map(|effort| json!({ "effort": effort, "description": effort_description(effort) }))
                .collect(),
        );
        changed = true;
    }
    if let Some(default) = model.default_effort {
        if unset(row, "default_reasoning_level") {
            row["default_reasoning_level"] = json!(default);
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_models_are_consistent() {
        for model in GEMINI_MODELS {
            assert!(model.slug.starts_with("gemini-"), "{}", model.slug);
            if let Some(default) = model.default_effort {
                assert!(model.efforts.contains(&default), "{}", model.slug);
            }
            assert!(!is_image_generation_model(model.slug));
        }
        assert_eq!(known_model("gemini-3-flash").unwrap().slug, "gemini-3-flash-preview");
        assert_eq!(known_model(" Gemini-2.5-Flash ").unwrap().slug, "gemini-2.5-flash");
        // Relay-only names are not guessed.
        for relay in ["gemini-2.5-flash-thinking", "gemini-3.8-flash-tiered", "gemini-3.8-flash-low"] {
            assert!(known_model(relay).is_none(), "{relay}");
        }
        assert!(is_image_generation_model("gemini-3-pro-image-preview"));
        assert!(is_image_generation_model("gemini-3.1-flash-image-preview"));
        assert!(!is_image_generation_model("imagen-4.0-generate"));
    }

    #[test]
    fn overlay_fills_unset_fields_only() {
        let mut row = json!({"slug": "gemini-3.8-flash", "context_window": null,
            "supported_reasoning_levels": [], "default_reasoning_level": null});
        assert!(apply_model_overlay(&mut row));
        assert_eq!(row["context_window"], 1_048_576);
        assert_eq!(row["input_modalities"], json!(["text", "image"]));
        assert_eq!(row["default_reasoning_level"], "medium");
        let efforts: Vec<_> = row["supported_reasoning_levels"].as_array().unwrap().iter()
            .map(|level| level["effort"].as_str().unwrap().to_string()).collect();
        assert_eq!(efforts, ["low", "medium", "high"]);
        assert!(!apply_model_overlay(&mut row), "idempotent");

        // Owner edits win.
        let mut edited = json!({"slug": "gemini-3-flash", "context_window": 200000,
            "input_modalities": ["text"], "default_reasoning_level": "low",
            "supported_reasoning_levels": [{"effort": "low", "description": "x"}]});
        let before = edited.clone();
        assert!(!apply_model_overlay(&mut edited));
        assert_eq!(edited, before);

        // No documented default level: left unset.
        let mut lite = json!({"slug": "gemini-2.5-flash-lite", "default_reasoning_level": null});
        assert!(apply_model_overlay(&mut lite));
        assert_eq!(lite["default_reasoning_level"], Value::Null);

        // Unknown / relay-only names are untouched.
        for slug in ["gemini-2.5-flash-thinking", "glm-5.3", "gpt-5.5"] {
            let mut other = json!({"slug": slug, "context_window": null});
            let before = other.clone();
            assert!(!apply_model_overlay(&mut other), "{slug}");
            assert_eq!(other, before);
        }
    }

    #[test]
    fn recommended_default_sorts_first_without_invented_metadata() {
        let mut row = json!({"slug": GEMINI_DEFAULT_MODEL, "priority": 0, "context_window": null});
        assert!(apply_model_overlay(&mut row));
        assert_eq!(row["priority"], -1);
        assert_eq!(row["context_window"], Value::Null);
        assert!(row.get("input_modalities").is_none());
        assert!(!apply_model_overlay(&mut row));
        let mut edited = json!({"slug": GEMINI_DEFAULT_MODEL, "priority": 7});
        assert!(!apply_model_overlay(&mut edited));
        assert_eq!(edited["priority"], 7);
    }

    #[test]
    fn image_generation_rows_are_hidden_once() {
        let mut row = json!({"slug": "gemini-3-pro-image-preview", "visibility": "list"});
        assert!(apply_model_overlay(&mut row));
        assert_eq!(row["visibility"], "hide");
        assert!(row.get("context_window").is_none());
        // The owner lists it again: the overlay does not re-hide it.
        row["visibility"] = json!("list");
        assert!(!apply_model_overlay(&mut row));
        assert_eq!(row["visibility"], "list");
    }

    #[test]
    fn preset_disables_the_builtin_web_search() {
        let mut root = Table::new();
        apply_profile_preset(&mut root);
        assert_eq!(root["web_search"].as_str(), Some("disabled"));
        assert!(is_gemini_provider(" Gemini "));
        assert!(!is_gemini_provider("custom"));
    }
}
