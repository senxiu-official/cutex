//! Persistence/launch regression fixtures use no real daemon or native binary.
use super::*;
use crate::profiles::{lifecycle::{create_profile, NewProfile, NewProfileKind}, settings_api};

pub(crate) fn fixture() -> (crate::profiles::lifecycle::tests::TestHome, StoredAccount, StockBundle, std::path::PathBuf) {
    let home = crate::profiles::lifecycle::tests::TestHome::new();
    let root = crate::config::paths::home_dir().unwrap();
    let native = root.join("native");
    std::fs::create_dir_all(&native).unwrap();
    let path = native.join("fixture-server");
    std::fs::write(&path, uuid::Uuid::new_v4().to_string()).unwrap();
    let file = VerifiedFile { path: path.canonicalize().unwrap(), sha256: crate::agent_management::file_sha256(&path).unwrap() };
    let bundle = StockBundle { version: 4, upstream_commit: "fixture".into(), native_patch_commit: None,
        executable: file.clone(), cli: None, code_mode_host: file.clone(), facade: file.clone(),
        schema: file.clone(), shared_config: file, launch_config_sha256: None };
    let manifest = native.join("bundle.json");
    std::fs::write(&manifest, serde_json::to_vec(&bundle).unwrap()).unwrap();
    let deployment = crate::launch::local_deployment::LocalDeployment { native_home: native.clone(), bundle_manifest: manifest, job_mcp: None };
    let light = crate::config::paths::runtime_dir().unwrap().join("light");
    std::fs::create_dir_all(&light).unwrap();
    std::fs::write(light.join("deployment.json"), serde_json::to_vec(&deployment).unwrap()).unwrap();
    let mut base = catalog_api::template_row("gpt-6-astra");
    base["context_window"] = json!(1_000_000);
    base["max_context_window"] = json!(1_000_000);
    base["base_instructions"] = json!("native instructions survive catalog edits");
    std::fs::write(native.join("models_cache.json"), serde_json::to_vec(&json!({"models": [base]})).unwrap()).unwrap();
    // This test models the memoized result of a successful probe. Parsing and
    // rejecting old/malformed capability replies are covered separately.
    set_support(&bundle, true);
    let account = create_profile(&NewProfile { name: "ChatGPT".into(), kind: NewProfileKind::Chatgpt,
        preset: None, base_url: None, model: Some("gpt-6-astra".into()), api_key: None }).unwrap();
    (home, account, bundle, native)
}

fn set_support(bundle: &StockBundle, supported: bool) {
    let key = probe_key(&bundle.executable, &["--model-variants-capability"]).unwrap();
    remember_support(key, supported).unwrap();
}

fn create_variant(account: &StoredAccount, id: &str, window: u64) {
    assert!(catalog_api::update_model(account, id, &ModelPatch {
        request_model: ConfigValueUpdate::Set("gpt-6-astra".into()),
        context_window: ConfigValueUpdate::Set(window), ..Default::default()
    }).unwrap());
}

#[test]
fn official_catalog_crud_preserves_credentials_metadata_and_immutable_files() {
    let (_home, account, _bundle, _native) = fixture();
    let files = crate::profiles::materialize::materialized_account_files(&account).unwrap();
    assert!(!files.auth_path.exists());
    let view = catalog_api::models_resource(&account).unwrap();
    assert_eq!(view["catalogSupported"], true);
    assert_eq!(view["discoverySupported"], false);
    assert_eq!(view["modelVariantsSupported"], true);
    assert_eq!(view["models"].as_array().unwrap().len(), 1);

    create_variant(&account, "Astra [400K]", 400_000);
    let first = catalog_api::load(&account).unwrap();
    let first_path = first.path.unwrap();
    let first_bytes = std::fs::read(&first_path).unwrap();
    create_variant(&account, "Astra [285K]", 285_000);
    let state = catalog_api::load(&account).unwrap();
    assert_ne!(state.path.as_ref(), Some(&first_path));
    assert_eq!(std::fs::read(&first_path).unwrap(), first_bytes);
    let catalog = state.catalog.unwrap();
    assert_eq!(catalog["models"].as_array().unwrap().len(), 3);
    assert_eq!(catalog["models"][2]["base_instructions"], "native instructions survive catalog edits");
    assert!(!files.auth_path.exists(), "catalog edits neither copy nor change credentials");

    let view = catalog_api::models_resource(&account).unwrap();
    assert_eq!(view["models"][1]["effective"]["contextWindow"], 400_000);
    assert_eq!(view["models"][2]["effective"]["contextWindow"], 285_000);
    assert_eq!(view["models"][2]["effective"]["autoCompactTokenLimit"], 256_500);
    assert!(catalog_api::discover(&account, false).is_err());
    catalog_api::reset_model(&account, "Astra [285K]").unwrap();
    let reset = catalog_api::load(&account).unwrap().catalog.unwrap();
    assert_eq!(reset["models"][2]["request_model"], "gpt-6-astra");
    assert_eq!(reset["models"][2]["display_name"], "Astra [285K]");
    assert_eq!(reset["models"][2]["context_window"], 1_000_000);
    catalog_api::remove_model(&account, "Astra [285K]").unwrap();
    assert_eq!(catalog_api::models_resource(&account).unwrap()["models"].as_array().unwrap().len(), 2);

    settings_api::update_profile(&account.id, &settings_api::ProfileApiPatch {
        codex_config: crate::profiles::codex_profile::CodexProfileConfigPatch {
            model: ConfigValueUpdate::Set("Astra [400K]".into()),
            model_context_window: ConfigValueUpdate::Set(200_000), ..Default::default()
        }, ..Default::default()
    }).unwrap();
    let view = catalog_api::models_resource(&account).unwrap();
    assert_eq!(view["models"][1]["effective"]["contextWindow"], 200_000);
    assert_eq!(view["models"][1]["effective"]["contextWindowSource"], "profile");
    assert!(catalog_api::remove_model(&account, "Astra [400K]").unwrap_err().to_string().contains("selected"));
    assert!(catalog_api::remove_model(&account, "gpt-6-astra").unwrap_err().to_string().contains("configurations reference"));
}

#[test]
fn official_launch_accepts_named_variants_and_downgrade_blocks_before_spawn() {
    let (_home, account, bundle, native) = fixture();
    create_variant(&account, "Astra [400K]", 400_000);
    let path = catalog_api::load(&account).unwrap().path.unwrap();
    let catalog = VerifiedFile { sha256: crate::agent_management::file_sha256(&path).unwrap(), path };
    crate::launch::selected_profile::validate_model(&Route::ChatgptFile, "Astra [400K]", None, Some(&catalog)).unwrap();
    assert!(crate::launch::selected_profile::validate_model(&Route::ChatgptFile, "missing", None, Some(&catalog)).is_err());
    let projection: Projection = serde_json::from_value(json!({
        "version": 2, "route": "chatgpt_file", "auth": {"path": native.join("unused-auth"),
            "parent_device": 0, "parent_inode": 0, "owner": 0, "account": null, "api_file": null},
        "settings": {}, "catalog": catalog, "requires_job": false
    })).unwrap();
    validate_runtime(&projection, &bundle, &native).unwrap();
    set_support(&bundle, false);
    assert!(validate_runtime(&projection, &bundle, &native).unwrap_err().to_string().contains("modelVariants support"));
    let view = catalog_api::models_resource(&account).unwrap();
    assert_eq!(view["catalogSupported"], false);
    assert_eq!(view["modelVariantsSupported"], false);
    assert!(catalog_api::update_model(&account, "Astra [400K]", &Default::default()).is_err());
    assert_eq!(view["models"][1]["requestModel"], "gpt-6-astra", "existing saved variants remain inspectable");
    set_support(&bundle, true);
    std::fs::write(&bundle.executable.path, "changed binary").unwrap();
    assert!(validate_runtime(&projection, &bundle, &native).is_err(), "memoized support never bypasses digest verification");
}

#[test]
fn duplicated_official_catalog_survives_source_deletion_without_credentials() {
    let (_home, source, _bundle, _native) = fixture();
    create_variant(&source, "Astra [400K]", 400_000);
    create_variant(&source, "Astra [285K]", 285_000);
    let source_files = crate::profiles::materialize::materialized_account_files(&source).unwrap();
    std::fs::write(&source_files.auth_path, br#"{"tokens":{"access_token":"fixture-only"}}"#).unwrap();
    let before_auth = std::fs::read(&source_files.auth_path).unwrap();
    let before = catalog_api::load(&source).unwrap();
    let copied = crate::profiles::lifecycle::duplicate_profile(&source.id, "ChatGPT copy").unwrap();
    let copy_files = crate::profiles::materialize::materialized_account_files(&copied).unwrap();
    let snapshot = catalog_api::load(&copied).unwrap();
    let copy_path = snapshot.path.unwrap();
    assert_ne!(Some(&copy_path), before.path.as_ref());
    assert_eq!(copy_path.parent(), Some(copy_files.config_path.parent().unwrap().canonicalize().unwrap().as_path()));
    assert!(copy_path.file_name().unwrap().to_string_lossy().starts_with("models-"));
    assert_eq!(snapshot.catalog, before.catalog);
    assert!(!copy_files.auth_path.exists(), "duplicate must require its own login");
    assert_eq!(std::fs::read(&source_files.auth_path).unwrap(), before_auth);

    crate::profiles::lifecycle::delete_profile(&source.id).unwrap();
    assert!(!before.path.unwrap().exists());
    assert_eq!(catalog_api::load(&copied).unwrap().catalog, snapshot.catalog);
    let view = catalog_api::models_resource(&copied).unwrap();
    assert_eq!(view["models"][1]["requestModel"], "gpt-6-astra");
    assert_eq!(view["models"][1]["effective"]["contextWindow"], 400_000);
    assert_eq!(view["models"][2]["effective"]["contextWindow"], 285_000);
    catalog_api::update_model(&copied, "Astra [285K]", &ModelPatch {
        context_window: ConfigValueUpdate::Set(300_000), ..Default::default()
    }).unwrap();
    assert!(copy_path.exists(), "later edits keep the duplicate's snapshot immutable");
    assert!(!copy_files.auth_path.exists());
}
