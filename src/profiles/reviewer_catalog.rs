//! Automatic-reviewer catalog for API-key profiles without a model catalog.
//!
//! Codex's automatic approval reviewer (`approvals_reviewer = "auto_review"`)
//! picks its model from the active catalog: the parent model's
//! `auto_review_model_override`, else the provider's preferred review model
//! (an OpenAI model, e.g. `gpt-5.6-luna` for API keys) when the catalog lists
//! it, else the agent's own model. A profile without `model_catalog_json`
//! runs on the native's bundled catalog, which lists the OpenAI review model.
//! On a provider the profile defines itself (any base URL that is not the
//! native's built-in OpenAI provider) that model usually does not exist: the
//! reviewer request fails (404) and every escalation is denied (fail closed).
//!
//! For such launches cutex hands the native a launch catalog instead of
//! nothing: the bundled rows of the same native (`<cli> debug models
//! --bundled`, so the metadata of every bundled model is unchanged), each with
//! `auto_review_model_override` set to its own slug, plus a row for the
//! launched model when the bundled list has none for it (the row the native
//! would have used: longest-prefix / namespaced match, else cutex's template
//! row with the native fallback's context window). The reviewer then reviews
//! with the agent's own model, which the provider serves. Profiles with a
//! catalog (discovered or hand-written) and the built-in OpenAI provider are
//! left alone. Generic: no provider, base URL or model is special-cased.
//!
//! The file is content-addressed (`reviewer-models-<sha256>.json` in the
//! profile directory, never rewritten).

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde_json::{json, Value};

use crate::launch::selected_profile::{Projection, Route};

pub const FILE_PREFIX: &str = "reviewer-models-";
/// Native fallback metadata's context window (`model_info_from_slug`).
const NATIVE_FALLBACK_CONTEXT_WINDOW: i64 = 272_000;

/// Whether a launch of `projection` needs the reviewer catalog: an API-key
/// route on a provider the profile defines, without a catalog of its own.
pub fn applies(projection: &Projection) -> bool {
    projection.route == Route::ApiKey && projection.catalog.is_none() && !projection.uses_builtin_openai_provider()
}

/// The launch catalog for `model` from the native's bundled rows.
pub fn catalog(bundled: &[Value], model: &str) -> Value {
    let mut rows: Vec<Value> = bundled
        .iter()
        .filter(|row| row.get("slug").and_then(Value::as_str).is_some())
        .cloned()
        .collect();
    for row in &mut rows {
        self_review(row);
    }
    if !rows.iter().any(|row| row["slug"].as_str() == Some(model)) {
        let mut row = match native_match(bundled, model) {
            Some(base) => {
                let mut row = base.clone();
                row["slug"] = json!(model);
                row
            }
            None => template(model),
        };
        row["auto_review_model_override"] = json!(model);
        rows.push(row);
    }
    json!({ "models": rows })
}

/// Write `catalog` into `dir` (content-addressed) and return its path.
pub fn write(dir: &Path, catalog: &Value) -> anyhow::Result<PathBuf> {
    use sha2::Digest;
    let mut bytes = serde_json::to_vec_pretty(catalog)?;
    bytes.push(b'\n');
    let path = dir.join(format!("{FILE_PREFIX}{:x}.json", sha2::Sha256::digest(&bytes)));
    let intact = std::fs::read(&path).is_ok_and(|existing| existing == bytes);
    if !intact {
        crate::config::atomic::write_private_bytes_atomic(&path, &bytes)?;
    }
    Ok(path.canonicalize()?)
}

/// The catalog file for a launch of `projection` with `model`, or `None` when
/// the profile does not need one. `bundled` is called only then.
pub fn for_launch(
    projection: &Projection,
    model: &str,
    bundled: impl FnOnce() -> anyhow::Result<Vec<Value>>,
) -> anyhow::Result<Option<PathBuf>> {
    if !applies(projection) {
        return Ok(None);
    }
    let rows = bundled().context("native bundled catalog unavailable")?;
    anyhow::ensure!(!rows.is_empty(), "native bundled catalog is empty");
    let dir = projection.auth.path.parent().context("profile directory missing")?;
    write(dir, &catalog(&rows, model)).map(Some)
}

/// `-c model_catalog_json=<file>` for a launch of `projection` with `model`
/// on `bundle` (whose CLI lists the bundled rows), or nothing. Best effort:
/// without the bundled list the launch stays as before (logged).
pub fn launch_args(
    projection: &Projection,
    model: &str,
    bundle: &crate::launch::stock::StockBundle,
    native_home: &Path,
) -> Vec<String> {
    let bundled = || match &bundle.cli {
        Some(cli) => crate::profiles::native_models::bundled_models(cli, native_home),
        None => anyhow::bail!("the native bundle has no CLI"),
    };
    match for_launch(projection, model, bundled) {
        Ok(Some(path)) => match toml::Value::try_from(path.to_string_lossy().into_owned()) {
            Ok(value) => vec!["-c".into(), format!("model_catalog_json={value}")],
            Err(_) => Vec::new(),
        },
        Ok(None) => Vec::new(),
        Err(error) => {
            eprintln!("cutex-reviewer-catalog: launch keeps the bundled catalog: {error:#}");
            Vec::new()
        }
    }
}

fn self_review(row: &mut Value) {
    let Some(slug) = row["slug"].as_str().map(str::to_string) else { return };
    let set = row.get("auto_review_model_override").and_then(Value::as_str).is_some_and(|v| !v.is_empty());
    if !set {
        row["auto_review_model_override"] = json!(slug);
    }
}

/// The bundled row the native resolves `model` to (models-manager
/// `find_model_by_longest_prefix`, then one namespace segment stripped).
fn native_match<'a>(bundled: &'a [Value], model: &str) -> Option<&'a Value> {
    let longest = |model: &str| -> Option<&'a Value> {
        bundled
            .iter()
            .filter(|row| {
                let Some(slug) = row["slug"].as_str() else { return false };
                let named = row.get("request_model").is_some_and(|v| !v.is_null());
                model.starts_with(slug) && (!named || model == slug)
            })
            .max_by_key(|row| row["slug"].as_str().map_or(0, str::len))
    };
    longest(model).or_else(|| {
        let (namespace, suffix) = model.split_once('/')?;
        let simple = !namespace.is_empty()
            && namespace.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if suffix.contains('/') || !simple {
            return None;
        }
        longest(suffix).filter(|row| row.get("request_model").is_none_or(Value::is_null))
    })
}

/// A model the bundled list does not know: cutex's template row (with the
/// documented metadata cutex knows), context window as the native fallback.
fn template(model: &str) -> Value {
    let catalog = crate::profiles::model_discovery::ModelCatalog::default();
    let mut row = crate::profiles::model_discovery::preset(&catalog, None, Some(model))["models"][0].clone();
    if row["context_window"].is_null() {
        row["context_window"] = json!(NATIVE_FALLBACK_CONTEXT_WINDOW);
        row["max_context_window"] = json!(NATIVE_FALLBACK_CONTEXT_WINDOW);
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundled() -> Vec<Value> {
        vec![
            json!({"slug":"gpt-5.5","context_window":400000,"supported_reasoning_levels":[{"effort":"low"}]}),
            json!({"slug":"gpt-5.6-luna","context_window":400000}),
            json!({"slug":"codex-auto-review","visibility":"hide","auto_review_model_override":"codex-auto-review"}),
        ]
    }

    fn row<'a>(catalog: &'a Value, slug: &str) -> &'a Value {
        catalog["models"].as_array().unwrap().iter().find(|row| row["slug"] == slug).unwrap()
    }

    #[test]
    fn bundled_rows_review_with_their_own_model() {
        let catalog = catalog(&bundled(), "gpt-5.5");
        assert_eq!(catalog["models"].as_array().unwrap().len(), 3);
        assert_eq!(row(&catalog, "gpt-5.5")["auto_review_model_override"], "gpt-5.5");
        assert_eq!(row(&catalog, "gpt-5.5")["context_window"], 400000);
        assert_eq!(row(&catalog, "gpt-5.6-luna")["auto_review_model_override"], "gpt-5.6-luna");
    }

    #[test]
    fn unknown_model_gets_a_self_reviewing_row() {
        let catalog = catalog(&bundled(), "acme-relay-model");
        let row = row(&catalog, "acme-relay-model");
        assert_eq!(row["auto_review_model_override"], "acme-relay-model");
        assert_eq!(row["context_window"], NATIVE_FALLBACK_CONTEXT_WINDOW);
        assert!(row["model_messages"]["instructions_template"].as_str().is_some_and(|t| !t.is_empty()));
    }

    #[test]
    fn prefix_and_namespaced_models_keep_the_native_match() {
        let catalog = catalog(&bundled(), "gpt-5.5-relay");
        let row = row(&catalog, "gpt-5.5-relay");
        assert_eq!(row["context_window"], 400000);
        assert_eq!(row["auto_review_model_override"], "gpt-5.5-relay");
        let catalog = super::catalog(&bundled(), "relay/gpt-5.5");
        assert_eq!(super::tests::row(&catalog, "relay/gpt-5.5")["context_window"], 400000);
    }

    #[test]
    fn write_is_content_addressed_and_idempotent() {
        let dir = std::env::temp_dir().join(format!("reviewer-catalog-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let catalog = catalog(&bundled(), "gpt-5.5");
        let first = write(&dir, &catalog).unwrap();
        let second = write(&dir, &catalog).unwrap();
        assert_eq!(first, second);
        let name = first.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with(FILE_PREFIX) && name.ends_with(".json"));
        let read: Value = serde_json::from_slice(&std::fs::read(&first).unwrap()).unwrap();
        assert_eq!(read, catalog);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
