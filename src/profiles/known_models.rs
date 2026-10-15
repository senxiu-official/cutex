//! Known-model metadata keyed by model slug (+ aliases), independent of the
//! provider that serves it: the official API and relays serving the same slug
//! (`glm-5.3`, `kimi-k3`, `deepseek-*`) get the same context window, input
//! types and reasoning efforts.
//!
//! Two uses:
//!
//! 1. [`apply_overlay`]: fill-only. Discovery and profile materialisation call
//!    it on every catalog row; it fills a field only while it is unset (absent,
//!    null or an empty list), so metadata the owner edited through the catalog
//!    API, or a vendor-shipped catalog, is never overwritten.
//! 2. [`catalog_json`]: the vendored catalog a preset writes at profile
//!    creation (native template row + every documented field of the entry,
//!    including the vendor's Codex-template fields `support_verbosity`,
//!    `truncation_policy` and `default_reasoning_summary`, which the
//!    fill-only overlay cannot touch because the template always sets them).
//!
//! Never instructions: rows keep native's default model prompt.
//!
//! Sources: `docs/research/profile-presets-upgrade-20261014.md` in
//! cutex-desktop (provider docs fetched 2026-10-14). Values not stated by an
//! official page are marked `unverified` next to the entry.
//!
//! Efforts must be exact per provider: native forwards the configured effort
//! as-is (only `ultra` / `persistent` are remapped), so a level the provider
//! does not document (e.g. `medium` on GLM / Kimi K3 / DeepSeek) must not be
//! offered. An empty effort list with no default means "send no effort".

use serde_json::{json, Value};

/// Documented metadata of one model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownModel {
    pub slug: &'static str,
    pub aliases: &'static [&'static str],
    pub display_name: &'static str,
    pub context_window: Option<i64>,
    pub max_context_window: Option<i64>,
    /// Image input (`input_modalities` text + image) or text only.
    pub image: bool,
    pub efforts: &'static [&'static str],
    pub default_effort: Option<&'static str>,
    pub supports_parallel_tool_calls: Option<bool>,
    pub supports_image_detail_original: Option<bool>,
    /// Vendor Codex-template fields (catalog building only).
    pub template: VendorTemplate,
}

/// Fields of a vendor's own Codex model template that differ from native's
/// discovery template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VendorTemplate {
    /// Native discovery template as is.
    Native,
    /// Z.ai / BigModel official Codex template: `support_verbosity:false`,
    /// `truncation_policy {bytes, 10000}`, `shell_type shell_command`,
    /// `apply_patch freeform`, `default_reasoning_summary none`.
    Glm,
}

const LOW_HIGH_MAX: &[&str] = &["low", "high", "max"];
const LOW_TO_MAX: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const LOW_TO_XHIGH: &[&str] = &["low", "medium", "high", "xhigh"];
const NONE: &[&str] = &[];
const ONE_M: i64 = 1_048_576;

const fn model(slug: &'static str, display_name: &'static str) -> KnownModel {
    KnownModel {
        slug,
        aliases: &[],
        display_name,
        context_window: None,
        max_context_window: None,
        image: false,
        efforts: NONE,
        default_effort: None,
        supports_parallel_tool_calls: None,
        supports_image_detail_original: None,
        template: VendorTemplate::Native,
    }
}

pub const KNOWN_MODELS: &[KnownModel] = &[
    // --- DeepSeek (api-docs.deepseek.com, pricing + Codex integration) -----
    // V4.1-Flash: 1,048,576, image input, low/high/max → high.
    KnownModel { context_window: Some(ONE_M), max_context_window: Some(ONE_M), image: true,
        efforts: LOW_HIGH_MAX, default_effort: Some("high"), supports_parallel_tool_calls: Some(true),
        ..model("deepseek-flash", "DeepSeek-Flash") },
    KnownModel { context_window: Some(ONE_M), max_context_window: Some(ONE_M),
        efforts: LOW_HIGH_MAX, default_effort: Some("high"), supports_parallel_tool_calls: Some(true),
        ..model("deepseek-v4-pro", "DeepSeek-V4-Pro") },
    // Retired name, still accepted (served by Flash); text only as in
    // DeepSeek's earlier catalog.
    KnownModel { context_window: Some(ONE_M), max_context_window: Some(ONE_M),
        efforts: LOW_HIGH_MAX, default_effort: Some("high"), supports_parallel_tool_calls: Some(true),
        ..model("deepseek-v4-flash", "DeepSeek-V4-Flash") },
    // --- Zhipu GLM (docs.z.ai guides + both vendors' Codex pages) ------------
    // glm-5.3: 1,048,576, text, low/high/max → max (thinking always on).
    KnownModel { context_window: Some(ONE_M), efforts: LOW_HIGH_MAX, default_effort: Some("max"),
        supports_parallel_tool_calls: Some(true), template: VendorTemplate::Glm,
        ..model("glm-5.3", "GLM-5.3") },
    // glm-5.3-flash: 1M, image (+ video / file) input. Only `max` is stated;
    // the full level list is unverified, so no list is offered and `max` is
    // the default.
    KnownModel { context_window: Some(ONE_M), image: true, default_effort: Some("max"),
        supports_parallel_tool_calls: Some(true), template: VendorTemplate::Glm,
        ..model("glm-5.3-flash", "GLM-5.3-Flash") },
    // glm-5.3-flashx: 1M multimodal (not on the Coding Plan); efforts unverified.
    KnownModel { context_window: Some(ONE_M), image: true,
        supports_parallel_tool_calls: Some(true), template: VendorTemplate::Glm,
        ..model("glm-5.3-flashx", "GLM-5.3-FlashX") },
    // glm-5.2: 1M, text. Efforts unverified (none offered).
    KnownModel { context_window: Some(ONE_M),
        supports_parallel_tool_calls: Some(true), template: VendorTemplate::Glm,
        ..model("glm-5.2", "GLM-5.2") },
    // glm-5-turbo (BigModel official template): 204,800, text,
    // `supported_reasoning_levels: []` with default `max`.
    KnownModel { context_window: Some(204_800), default_effort: Some("max"),
        supports_parallel_tool_calls: Some(true), template: VendorTemplate::Glm,
        ..model("glm-5-turbo", "GLM-5-Turbo") },
    // --- Moonshot / Kimi (platform.kimi.ai docs) ----------------------------
    // kimi-k3: 1,048,576, image, low/high/max → max.
    KnownModel { context_window: Some(ONE_M), image: true, efforts: LOW_HIGH_MAX, default_effort: Some("max"),
        ..model("kimi-k3", "Kimi K3") },
    // k2.x: 256k (262,144 tokens assumed: unverified exact count), image, no
    // reasoning_effort (always reasons / `thinking` param): no effort is sent.
    KnownModel { context_window: Some(262_144), image: true, ..model("kimi-k2.7-code", "Kimi K2.7 Code") },
    KnownModel { context_window: Some(262_144), image: true,
        ..model("kimi-k2.7-code-highspeed", "Kimi K2.7 Code Highspeed") },
    KnownModel { context_window: Some(262_144), image: true, ..model("kimi-k2.6", "Kimi K2.6") },
    // --- MiniMax (platform.minimax.io Token Plan Codex guide) ----------------
    // 524,288 recommended (1M option), image, low…max → max, never `none`;
    // supports_image_detail_original false; parallel calls supported.
    KnownModel { context_window: Some(524_288), max_context_window: Some(ONE_M), image: true,
        efforts: LOW_TO_MAX, default_effort: Some("max"), supports_parallel_tool_calls: Some(true),
        supports_image_detail_original: Some(false),
        ..model("MiniMax-M3.1-Flash-Preview", "MiniMax M3.1 Flash (Preview)") },
    KnownModel { context_window: Some(524_288), max_context_window: Some(ONE_M), image: true,
        efforts: LOW_TO_MAX, default_effort: Some("max"), supports_parallel_tool_calls: Some(true),
        supports_image_detail_original: Some(false),
        ..model("MiniMax-M3", "MiniMax M3") },
    // --- xAI (docs.x.ai models + reasoning) ----------------------------------
    // grok-4.7 / 4.6: "500k" context (500,000 assumed: unverified exact
    // count), image, low/medium/high/xhigh → high.
    KnownModel { context_window: Some(500_000), image: true, efforts: LOW_TO_XHIGH, default_effort: Some("high"),
        ..model("grok-4.7", "Grok 4.7") },
    KnownModel { context_window: Some(500_000), image: true, efforts: LOW_TO_XHIGH, default_effort: Some("high"),
        ..model("grok-4.6", "Grok 4.6") },
    // grok-4.3: "1M" context (1,000,000 assumed: unverified exact count);
    // image input and efforts unverified (text only, no effort offered).
    KnownModel { context_window: Some(1_000_000), ..model("grok-4.3", "Grok 4.3") },
];

pub fn known_model(slug: &str) -> Option<&'static KnownModel> {
    let slug = slug.trim();
    KNOWN_MODELS.iter().find(|model| {
        model.slug.eq_ignore_ascii_case(slug)
            || model.aliases.iter().any(|alias| alias.eq_ignore_ascii_case(slug))
    })
}

pub fn effort_description(effort: &str) -> &'static str {
    match effort {
        "none" => "No reasoning",
        "minimal" => "Minimal reasoning",
        "low" => "Fast responses with lighter reasoning",
        "medium" => "Balanced reasoning depth",
        "high" => "Extra high reasoning depth for complex problems",
        "xhigh" => "Very deep reasoning",
        "max" => "Maximum reasoning depth for the hardest problems",
        _ => "Reasoning effort",
    }
}

pub fn effort_levels(efforts: &[&str]) -> Value {
    Value::Array(
        efforts
            .iter()
            .map(|effort| json!({ "effort": effort, "description": effort_description(effort) }))
            .collect(),
    )
}

fn input_modalities(image: bool) -> Value {
    if image {
        json!(["text", "image"])
    } else {
        json!(["text"])
    }
}

pub(crate) fn unset(row: &Value, key: &str) -> bool {
    match row.get(key) {
        None | Some(Value::Null) => true,
        Some(Value::Array(items)) => items.is_empty(),
        _ => false,
    }
}

/// Fill documented metadata into the unset fields of a known model's row.
/// Returns whether anything changed.
pub fn apply_overlay(row: &mut Value) -> bool {
    let Some(model) = row.get("slug").and_then(Value::as_str).and_then(known_model) else {
        return false;
    };
    if !row.is_object() {
        return false;
    }
    let mut changed = false;
    let fill = |row: &mut Value, key: &str, value: Value, changed: &mut bool| {
        if unset(row, key) {
            row[key] = value;
            *changed = true;
        }
    };
    if let Some(window) = model.context_window {
        fill(row, "context_window", json!(window), &mut changed);
    }
    if let Some(window) = model.max_context_window {
        fill(row, "max_context_window", json!(window), &mut changed);
    }
    // Absent only: an explicit list (even text-only) is the row's own value.
    if row.get("input_modalities").is_none() {
        row["input_modalities"] = input_modalities(model.image);
        changed = true;
    }
    if !model.efforts.is_empty() {
        fill(row, "supported_reasoning_levels", effort_levels(model.efforts), &mut changed);
    }
    if let Some(default) = model.default_effort {
        fill(row, "default_reasoning_level", json!(default), &mut changed);
    }
    if let Some(parallel) = model.supports_parallel_tool_calls {
        fill(row, "supports_parallel_tool_calls", json!(parallel), &mut changed);
    }
    if let Some(original) = model.supports_image_detail_original {
        fill(row, "supports_image_detail_original", json!(original), &mut changed);
    }
    changed
}

/// A full catalog row for a known model: native's discovery template with
/// every documented field set (not fill-only).
pub fn catalog_row(model: &KnownModel, priority: i64) -> Value {
    let mut row = crate::profiles::model_discovery::template_row(model.slug);
    row["display_name"] = json!(model.display_name);
    row["priority"] = json!(priority);
    row["context_window"] = json!(model.context_window);
    if let Some(window) = model.max_context_window {
        row["max_context_window"] = json!(window);
    }
    row["input_modalities"] = input_modalities(model.image);
    row["supported_reasoning_levels"] = effort_levels(model.efforts);
    row["default_reasoning_level"] = json!(model.default_effort);
    if let Some(parallel) = model.supports_parallel_tool_calls {
        row["supports_parallel_tool_calls"] = json!(parallel);
    }
    if let Some(original) = model.supports_image_detail_original {
        row["supports_image_detail_original"] = json!(original);
    }
    if model.template == VendorTemplate::Glm {
        row["support_verbosity"] = json!(false);
        row["default_verbosity"] = Value::Null;
        row["truncation_policy"] = json!({ "mode": "bytes", "limit": 10000 });
        row["shell_type"] = json!("shell_command");
        row["apply_patch_tool_type"] = json!("freeform");
        row["default_reasoning_summary"] = json!("none");
    }
    row
}

/// Catalog JSON (`{"models": [...]}`) of the given known slugs, in order
/// (the first sorts first in pickers).
pub fn catalog_json(slugs: &[&str]) -> String {
    let rows: Vec<Value> = slugs
        .iter()
        .enumerate()
        .map(|(index, slug)| {
            let model = known_model(slug).unwrap_or_else(|| panic!("{slug} is not a known model"));
            catalog_row(model, index as i64 + 1)
        })
        .collect();
    serde_json::to_string_pretty(&json!({ "models": rows })).expect("catalog serializes")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Native r51 `ModelInfo` shape checks (cute-codex
    /// `protocol/src/openai_models.rs`): required keys, enum values, an
    /// instructions template, and a default effort inside a non-empty list.
    pub(crate) fn assert_native_model_rows(catalog: &Value) {
        const EFFORTS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra", "persistent"];
        let rows = catalog["models"].as_array().expect("models array");
        assert!(!rows.is_empty());
        for row in rows {
            let slug = row["slug"].as_str().expect("slug");
            for key in ["display_name", "supported_reasoning_levels", "shell_type", "visibility",
                "supported_in_api", "priority", "support_verbosity", "truncation_policy", "experimental_supported_tools"] {
                assert!(row.get(key).is_some_and(|v| !v.is_null()), "{slug}: {key}");
            }
            assert!(
                row["model_messages"]["instructions_template"].as_str().is_some_and(|t| !t.is_empty())
                    || row["base_instructions"].as_str().is_some(),
                "{slug}: instructions"
            );
            assert!(["unified_exec", "shell_command", "default", "local", "disabled"]
                .contains(&row["shell_type"].as_str().unwrap()), "{slug}");
            assert!(["list", "hide", "none"].contains(&row["visibility"].as_str().unwrap()), "{slug}");
            assert!(["bytes", "tokens"].contains(&row["truncation_policy"]["mode"].as_str().unwrap()), "{slug}");
            let levels: Vec<&str> = row["supported_reasoning_levels"].as_array().unwrap().iter()
                .map(|level| {
                    assert!(level["description"].is_string(), "{slug}");
                    level["effort"].as_str().unwrap()
                })
                .collect();
            for level in &levels {
                assert!(EFFORTS.contains(level), "{slug}: {level}");
            }
            if let Some(default) = row["default_reasoning_level"].as_str() {
                assert!(EFFORTS.contains(&default), "{slug}");
                assert!(levels.is_empty() || levels.contains(&default), "{slug}: default {default} not in {levels:?}");
            }
            if let Some(modalities) = row.get("input_modalities").and_then(Value::as_array) {
                for modality in modalities {
                    assert!(["text", "image"].contains(&modality.as_str().unwrap()), "{slug}");
                }
            }
        }
    }

    #[test]
    fn table_is_consistent() {
        for model in KNOWN_MODELS {
            if let (Some(default), false) = (model.default_effort, model.efforts.is_empty()) {
                assert!(model.efforts.contains(&default), "{}", model.slug);
            }
            assert!(!model.efforts.contains(&"none"), "{}", model.slug);
            assert_eq!(known_model(&model.slug.to_uppercase()).unwrap().slug, model.slug);
        }
        // GLM / Kimi K3 / DeepSeek document only low/high/max.
        for slug in ["glm-5.3", "kimi-k3", "deepseek-flash", "deepseek-v4-pro"] {
            assert_eq!(known_model(slug).unwrap().efforts, LOW_HIGH_MAX, "{slug}");
        }
        let all: Vec<&str> = KNOWN_MODELS.iter().map(|model| model.slug).collect();
        assert_native_model_rows(&serde_json::from_str(&catalog_json(&all)).unwrap());
    }

    #[test]
    fn overlay_fills_unset_fields_only_and_is_idempotent() {
        let mut row = crate::profiles::model_discovery::template_row("glm-5.3");
        assert!(apply_overlay(&mut row));
        assert_eq!(row["context_window"], ONE_M);
        assert_eq!(row["input_modalities"], json!(["text"]));
        assert_eq!(row["default_reasoning_level"], "max");
        let efforts: Vec<_> = row["supported_reasoning_levels"].as_array().unwrap().iter()
            .map(|level| level["effort"].as_str().unwrap()).collect();
        assert_eq!(efforts, ["low", "high", "max"]);
        assert!(!apply_overlay(&mut row));

        let mut edited = json!({"slug": "kimi-k3", "context_window": 200000, "input_modalities": ["text"],
            "default_reasoning_level": "low", "supported_reasoning_levels": [{"effort": "low", "description": "x"}]});
        let before = edited.clone();
        assert!(!apply_overlay(&mut edited));
        assert_eq!(edited, before);

        // No efforts for Kimi K2.x: nothing invented.
        let mut k2 = json!({"slug": "kimi-k2.6", "supported_reasoning_levels": [], "default_reasoning_level": null});
        assert!(apply_overlay(&mut k2));
        assert_eq!(k2["supported_reasoning_levels"], json!([]));
        assert_eq!(k2["default_reasoning_level"], Value::Null);
        assert_eq!(k2["input_modalities"], json!(["text", "image"]));

        for unknown in ["gpt-6-astra", "glm-9", "x"] {
            let mut other = json!({"slug": unknown, "context_window": null});
            assert!(!apply_overlay(&mut other), "{unknown}");
        }
    }

    #[test]
    fn glm_catalog_rows_follow_the_vendor_template() {
        let catalog: Value = serde_json::from_str(&catalog_json(&["glm-5.3", "glm-5-turbo"])).unwrap();
        let glm = &catalog["models"][0];
        assert_eq!(glm["truncation_policy"], json!({"mode": "bytes", "limit": 10000}));
        assert_eq!(glm["support_verbosity"], false);
        assert_eq!(glm["default_reasoning_summary"], "none");
        assert_eq!(glm["priority"], 1);
        let turbo = &catalog["models"][1];
        assert_eq!(turbo["supported_reasoning_levels"], json!([]));
        assert_eq!(turbo["default_reasoning_level"], "max");
        assert_eq!(turbo["context_window"], 204_800);
    }
}
