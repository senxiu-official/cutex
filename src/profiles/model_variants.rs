//! Local model selections keep native metadata and a distinct wire model ID.
//! Support is checked against the verified server, including a pinned old runtime.
use std::{path::Path, sync::Mutex};

use anyhow::{ensure, Context};
use serde_json::{json, Value};

use crate::config::global_settings::ConfigValueUpdate;
use crate::launch::{selected_profile::{Projection, Route}, stock::{StockBundle, VerifiedFile}};
use super::{catalog_api::{self, CatalogState, ModelPatch}, model::{CliKind, StoredAccount}};

/// Probe result for one verified executable. The key is the expected digest,
/// the probe arguments and the file's (path, size, modification time) when it
/// was verified: while those are unchanged the ~260 MB server is neither
/// re-hashed nor re-run; any change re-verifies the digest and re-probes.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ProbeKey {
    sha256: String,
    args: String,
    path: std::path::PathBuf,
    len: u64,
    modified: Option<std::time::SystemTime>,
}

/// One entry per (path, probe arguments): the selected runtime and explicitly
/// pinned older ones do not evict each other.
type SupportCache = std::collections::HashMap<(std::path::PathBuf, String), (ProbeKey, bool)>;
static SUPPORT: Mutex<Option<SupportCache>> = Mutex::new(None);

fn remember_support(key: ProbeKey, supported: bool) -> anyhow::Result<()> {
    let mut cache = SUPPORT.lock().map_err(|_| anyhow::anyhow!("model capability cache poisoned"))?;
    cache.get_or_insert_with(Default::default).insert((key.path.clone(), key.args.clone()), (key, supported));
    Ok(())
}

fn probe_key(cli: &VerifiedFile, args: &[&str]) -> Option<ProbeKey> {
    let metadata = std::fs::metadata(&cli.path).ok().filter(|metadata| metadata.is_file())?;
    Some(ProbeKey {
        sha256: cli.sha256.as_str().to_string(),
        args: args.join(" "),
        path: cli.path.clone(),
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

fn parse_support(bytes: &[u8]) -> bool {
    serde_json::from_slice::<Value>(bytes).ok()
        .and_then(|value| value.get("modelVariants").and_then(Value::as_bool)) == Some(true)
}

fn cached_support(key: &ProbeKey) -> anyhow::Result<Option<bool>> {
    let cache = SUPPORT.lock().map_err(|_| anyhow::anyhow!("model capability cache poisoned"))?;
    Ok(cache
        .as_ref()
        .and_then(|cache| cache.get(&(key.path.clone(), key.args.clone())))
        .filter(|(cached, _)| cached == key)
        .map(|(_, supported)| *supported))
}

fn supported(cli: &VerifiedFile, native_home: &Path, args: &[&str]) -> anyhow::Result<bool> {
    let key = probe_key(cli, args).context("native executable is missing")?;
    if let Some(supported) = cached_support(&key)? {
        return Ok(supported);
    }
    // A new or changed file: verify its digest before trusting the probe, so
    // swapping the file fails closed.
    cli.validate()?;
    let supported = super::native_models::run_cli(cli, args, native_home,
        std::time::Duration::from_secs(10)).map(|out| parse_support(&out)).unwrap_or(false);
    // Remember it only if the file did not change while it was verified.
    if probe_key(cli, args).as_ref() == Some(&key) {
        remember_support(key, supported)?;
    }
    Ok(supported)
}

pub fn bundle_supported(bundle: &StockBundle, native_home: &Path) -> anyhow::Result<bool> {
    // Frontend-only upgrades may pair a new CLI with an old app-server. Probe
    // the actual server, not the frontend that happened to build the catalog.
    if bundle.common_ingress() {
        supported(&bundle.executable, native_home, &["--model-variants-capability"])
    } else {
        supported(&bundle.executable, native_home, &["debug", "models", "--capabilities"])
    }
}

pub fn selected_supported() -> bool {
    (|| -> anyhow::Result<bool> {
        let deployment = crate::launch::local_deployment::LocalDeployment::selected()?
            .context("no local runtime selected")?;
        let bundle: StockBundle = serde_json::from_slice(&std::fs::read(&deployment.bundle_manifest)?)?;
        bundle_supported(&bundle, &deployment.native_home)
    })().unwrap_or(false)
}

pub fn for_profile(account: &StoredAccount) -> bool {
    account.cli_kind == CliKind::Codex && selected_supported()
}

fn needs_support(route: &Route, rows: &[Value]) -> bool {
    *route == Route::ChatgptFile || rows.iter().any(|row| row.get("request_model").is_some_and(|v| !v.is_null()))
}

/// Called with the actual launch bundle, not the currently selected deployment.
pub fn validate_runtime(projection: &Projection, bundle: &StockBundle, native_home: &Path) -> anyhow::Result<()> {
    let Some(catalog) = &projection.catalog else { return Ok(()); };
    catalog.validate()?;
    let bytes = std::fs::read(&catalog.path)?;
    use sha2::Digest;
    ensure!(format!("{:x}", sha2::Sha256::digest(&bytes)) == catalog.sha256.as_str(), "catalog changed before runtime capability check");
    let catalog: Value = serde_json::from_slice(&bytes)?;
    let rows = catalog["models"].as_array().context("catalog models missing")?;
    validate_bindings(rows)?;
    if needs_support(&projection.route, rows) {
        ensure!(bundle_supported(bundle, native_home)?,
            "this model catalog requires a native runtime with modelVariants support; update the selected or pinned runtime");
    }
    Ok(())
}

pub fn validate_bindings(rows: &[Value]) -> anyhow::Result<()> {
    for row in rows {
        let Some(target) = row.get("request_model").filter(|v| !v.is_null()) else { continue; };
        let target = target.as_str().context("requestModel must be a model ID")?;
        catalog_api::validate_model_id(target)?;
        ensure!(row["slug"].as_str() != Some(target), "a model configuration cannot target itself");
        let bases: Vec<_> = rows.iter().filter(|base| base["slug"].as_str() == Some(target)).collect();
        ensure!(bases.len() == 1 && bases[0].get("request_model").is_none_or(Value::is_null),
            "requestModel must reference one base model, not another configuration");
    }
    Ok(())
}

pub fn catalog_for_edit(account: &StoredAccount, state: &CatalogState) -> anyhow::Result<Value> {
    if let Some(catalog) = &state.catalog { return Ok(catalog.clone()); }
    if catalog_api::discovery_supported(account) { return Ok(json!({"models": []})); }
    let models = super::native_models::for_profile(account, state.snapshot.model.as_deref())
        .map_err(anyhow::Error::msg)?;
    Ok(json!({"models": models.rows}))
}

/// Clone every native field, including instructions and tools, before overrides.
/// The wire binding of an existing slug is immutable.
pub fn patch_row(rows: &mut Vec<Value>, id: &str, patch: &ModelPatch, official: bool) -> anyhow::Result<bool> {
    let existing = rows.iter().position(|row| row["slug"].as_str() == Some(id));
    let target = match &patch.request_model {
        ConfigValueUpdate::Set(target) => Some(target.as_str()),
        ConfigValueUpdate::Clear => None,
        ConfigValueUpdate::Unchanged => existing.and_then(|index| rows[index]["request_model"].as_str()),
    };
    if let Some(index) = existing {
        ensure!(rows[index]["request_model"].as_str() == target, "requestModel cannot change for an existing model ID");
    }
    let mut row = match existing {
        Some(index) => rows[index].clone(),
        None => match target {
            Some(target) => {
                catalog_api::validate_model_id(target)?;
                ensure!(target != id, "a model configuration cannot target itself");
                let base = rows.iter().find(|row| row["slug"].as_str() == Some(target))
                    .context("requestModel must reference a known base model")?;
                ensure!(base.get("request_model").is_none_or(Value::is_null), "a model configuration cannot target another configuration");
                let mut row = base.clone();
                row["slug"] = json!(id);
                row["display_name"] = json!(id);
                row["request_model"] = json!(target);
                row["visibility"] = json!("list");
                row
            }
            None => {
                ensure!(!official, "official model additions require requestModel from the native catalog");
                catalog_api::template_row(id)
            }
        },
    };
    catalog_api::apply_model_patch(&mut row, patch)?;
    match existing { Some(index) => rows[index] = row, None => rows.push(row) }
    validate_bindings(rows)?;
    Ok(existing.is_none())
}

pub fn reset_row(current: &Value, mut fresh: Value) -> Value {
    if current["request_model"].is_string() {
        for key in ["slug", "request_model", "display_name"] { fresh[key] = current[key].clone(); }
        fresh["visibility"] = json!("list");
    }
    fresh
}

/// Reset limits and native metadata without changing a configuration identity.
pub(super) fn reset_catalog_row(account: &StoredAccount, state: &CatalogState, id: &str, catalog: &mut Value) -> anyhow::Result<()> {
    let current = catalog["models"].as_array_mut().context("catalog models missing")?.iter().find(|row| row["slug"].as_str() == Some(id))
        .cloned().with_context(|| format!("model {id} is not in the catalog"))?;
    let base = current["request_model"].as_str().unwrap_or(id);
    let native = (!catalog_api::discovery_supported(account)).then(|| crate::profiles::native_models::for_profile(account, Some(base)))
        .transpose().map_err(anyhow::Error::msg)?;
    let native_row = native.and_then(|models| models.rows.into_iter().find(|row| row["slug"].as_str() == Some(base)));
    ensure!(catalog_api::discovery_supported(account) || native_row.is_some(), "the base model is not in the native catalog");
    // The preset's own row (DeepSeek, GLM, Kimi, MiniMax, xAI catalogs).
    let preset_row = state
        .snapshot
        .model_provider
        .as_deref()
        .and_then(|provider| crate::profiles::preset_spec::catalog_row_for_provider(provider, base));
    let custom_base = (current["request_model"].is_string() && catalog_api::discovery_supported(account))
        .then(|| catalog["models"].as_array_mut()?.iter().find(|row| row["slug"].as_str() == Some(base)).cloned())
        .flatten();
    let mut fresh = native_row.or(preset_row).or(custom_base).unwrap_or_else(|| catalog_api::template_row(base));
    let mut wrapper = json!({ "models": [fresh.take()] });
    super::model_discovery::apply_known_model_metadata_overlays_to_catalog(&mut wrapper);
    let fresh = crate::profiles::model_variants::reset_row(&current, wrapper["models"][0].take());
    let rows = catalog["models"].as_array_mut().context("catalog models missing")?;
    match rows.iter_mut().find(|row| row.get("slug").and_then(Value::as_str) == Some(id)) {
        Some(row) => *row = fresh,
        None => anyhow::bail!("model {id} is not in the catalog"),
    }
    Ok(())
}

#[cfg(test)]
#[path = "model_variants_store_tests.rs"]
pub(crate) mod store_tests;

#[cfg(test)]
mod tests {
    use super::*;
    fn base() -> Value {
        let mut row = catalog_api::template_row("gpt-6-astra");
        row["context_window"] = json!(1_000_000);
        row["max_context_window"] = json!(1_000_000);
        row["base_instructions"] = json!("native instructions");
        row
    }
    fn variant(window: u64) -> ModelPatch {
        ModelPatch { request_model: ConfigValueUpdate::Set("gpt-6-astra".into()),
            context_window: ConfigValueUpdate::Set(window), ..Default::default() }
    }
    #[test]
    fn variants_clone_native_fields_and_keep_independent_limits() {
        let mut rows = vec![base()];
        assert!(patch_row(&mut rows, "Astra [400K]", &variant(400_000), true).unwrap());
        assert!(patch_row(&mut rows, "Astra [285K]", &variant(285_000), true).unwrap());
        assert_eq!(rows[1]["request_model"], "gpt-6-astra");
        assert_eq!(rows[2]["base_instructions"], "native instructions");
        assert_eq!(rows[1]["context_window"], 400_000);
        assert_eq!(rows[2]["context_window"], 285_000);
        assert_eq!(rows[0]["context_window"], 1_000_000);
        assert!(!patch_row(&mut rows, "Astra [400K]", &variant(410_000), true).unwrap());
        let view = catalog_api::model_view(&rows[2], &Default::default(), &[]);
        assert_eq!(view["requestModel"], "gpt-6-astra");
        assert_eq!(view["effective"]["autoCompactTokenLimit"], 256_500);
    }
    #[test]
    fn bindings_reject_unknown_self_chained_and_mutated_targets() {
        let mut rows = vec![base()];
        patch_row(&mut rows, "Astra [400K]", &variant(400_000), true).unwrap();
        for target in ["missing", "new", "Astra [400K]"] {
            let patch = ModelPatch { request_model: ConfigValueUpdate::Set(target.into()), ..Default::default() };
            assert!(patch_row(&mut rows, "new", &patch, true).is_err());
        }
        let clear = ModelPatch { request_model: ConfigValueUpdate::Clear, ..Default::default() };
        assert!(patch_row(&mut rows, "Astra [400K]", &clear, true).is_err());
        assert!(patch_row(&mut rows, "unknown", &Default::default(), true).is_err());
        rows.remove(0);
        assert!(validate_bindings(&rows).is_err());
    }
    #[test]
    fn reset_retains_configuration_identity_but_restores_native_limits() {
        let mut rows = vec![base()];
        patch_row(&mut rows, "Astra [285K]", &variant(285_000), true).unwrap();
        let reset = reset_row(&rows[1], base());
        assert_eq!(reset["slug"], "Astra [285K]");
        assert_eq!(reset["display_name"], "Astra [285K]");
        assert_eq!(reset["request_model"], "gpt-6-astra");
        assert_eq!(reset["context_window"], 1_000_000);
    }
    #[test]
    fn probe_is_cached_by_path_size_and_mtime_and_rehashed_only_when_they_change() {
        let dir = std::env::temp_dir().join(format!("cutex-probe-cache-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fixture-server");
        std::fs::write(&path, b"original-bytes").unwrap();
        let file = VerifiedFile { sha256: crate::agent_management::file_sha256(&path).unwrap(), path: path.canonicalize().unwrap() };
        let args = ["--probe-cache-test"];
        // The fixture cannot run: the probe reports no support, verified once.
        assert!(!supported(&file, &dir, &args).unwrap());
        let stamp = std::fs::metadata(&path).unwrap().modified().unwrap();
        // Same size and modification time: the cached answer is used and the
        // file is not hashed again (its new bytes would fail verification).
        std::fs::write(&path, b"replaced-bytes").unwrap();
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(stamp).unwrap();
        assert!(!supported(&file, &dir, &args).unwrap());
        // A new modification time re-verifies the digest: fails closed.
        std::fs::File::options().write(true).open(&path).unwrap()
            .set_modified(stamp + std::time::Duration::from_secs(5)).unwrap();
        assert!(supported(&file, &dir, &args).is_err());
        std::fs::write(&path, b"original-bytes").unwrap();
        assert!(!supported(&file, &dir, &args).unwrap(), "restored bytes verify again");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn old_native_capabilities_fail_closed_and_base_catalogs_remain_compatible() {
        assert!(parse_support(br#"{"modelVariants":true}"#));
        for invalid in [br#"{"modelVariants":false}"#.as_slice(), b"{}", b"error", br#"{"modelVariants":"true"}"#] {
            assert!(!parse_support(invalid));
        }
        assert!(needs_support(&Route::ChatgptFile, &[base()]));
        assert!(!needs_support(&Route::ApiKey, &[base()]));
        let mut rows = vec![base()];
        patch_row(&mut rows, "Astra [400K]", &variant(400_000), false).unwrap();
        assert!(needs_support(&Route::ApiKey, &rows));
    }
}
