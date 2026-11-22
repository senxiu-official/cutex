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
    set_context_support(&bundle, false);
    let account = create_profile(&NewProfile { name: "ChatGPT".into(), kind: NewProfileKind::Chatgpt,
        preset: None, base_url: None, model: Some("gpt-6-astra".into()), api_key: None }).unwrap();
    (home, account, bundle, native)
}

fn set_support(bundle: &StockBundle, supported: bool) {
    let key = probe_key(&bundle.executable, &["--model-variants-capability"]).unwrap();
    remember_support(key, supported).unwrap();
}

// r290 P13: fixture executables are data; never execute them as real probes.
pub(crate) fn set_context_support(bundle: &StockBundle, supported: bool) {
    let mut key = probe_key(&bundle.executable, &["--model-variants-capability"]).unwrap();
    key.args.push_str("#modelContextSettingsV2");
    remember_support(key, supported).unwrap();
}

fn create_variant(account: &StoredAccount, id: &str, window: u64) {
    assert!(catalog_api::update_model(account, id, &ModelPatch {
        request_model: ConfigValueUpdate::Set("gpt-6-astra".into()),
        context_window: ConfigValueUpdate::Set(window), ..Default::default()
    }).unwrap());
}

#[test]
fn migrated_profile_keeps_old_pins_startable_beside_a_v2_selected_runtime() {
    let (_home, account, selected, native) = fixture();
    set_context_support(&selected, true);
    assert!(crate::profiles::catalog_overlay::ensure_context_migrated(&account).unwrap());
    let snapshot = settings_api::codex_snapshot(&account);
    assert!(snapshot.model_context_settings_v1);
    assert!(snapshot.model_catalog_json.is_none(), "plain migration adds no catalog");
    let projection: Projection = serde_json::from_value(json!({
        "version":2, "route":"chatgpt_file", "auth":{"path":native.join("unused-auth"),
        "parent_device":0,"parent_inode":0,"owner":0,"account":null,"api_file":null},
        "settings":{"model_context_settings_v1":true}, "catalog":null,"requires_job":false
    })).unwrap();
    assert_eq!(projection_for_runtime(&projection, &selected, &native).unwrap(), projection);
    for version in ["r61", "r62"] {
        let path = native.join(format!("old-{version}"));
        std::fs::write(&path, version).unwrap();
        let mut pinned = selected.clone();
        pinned.executable = VerifiedFile { sha256: crate::agent_management::file_sha256(&path).unwrap(), path };
        set_context_support(&pinned, false);
        let launch = projection_for_runtime(&projection, &pinned, &native).unwrap();
        assert_eq!(launch.settings.model_context_settings_v1, Some(false));
        assert!(settings_api::codex_snapshot(&account).model_context_settings_v1);
        assert!(context_supported(&account), "selected support does not override the pin's result");
        let mut percent = projection.clone();
        percent.settings.model_auto_compact_percent = Some(75);
        assert!(projection_for_runtime(&percent, &pinned, &native).unwrap_err().to_string().contains("Profile compaction percentage"));
    }
}

#[test]
fn old_pin_requires_equivalent_limits_for_every_model_and_checks_catalog_digest() {
    let (_home, _account, old, native) = fixture();
    let row = json!({"slug":"model-a","context_window":100000,"max_context_window":200000,
        "context_settings":{"catalog":{"context_window":100000},"owner":{"context_window":50000},
        "preserved":{"context_window":100000}}});
    let path = native.join("context-catalog.json");
    let write = |value: Value| {
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        VerifiedFile { path:path.clone(),sha256:crate::agent_management::file_sha256(&path).unwrap() }
    };
    let catalog = write(json!({"models":[row.clone()]}));
    let mut projection: Projection = serde_json::from_value(json!({
        "version":2,"route":"chatgpt_file","auth":{"path":native.join("unused-auth"),
        "parent_device":0,"parent_inode":0,"owner":0,"account":null,"api_file":null},
        "settings":{"model_context_settings_v1":true},"catalog":catalog,"requires_job":false
    })).unwrap();
    let launch = projection_for_runtime(&projection,&old,&native).unwrap();
    assert_eq!(launch.settings.model_context_settings_v1,Some(false));
    assert_eq!(launch.catalog,projection.catalog,"no catalog rewrite");
    // Even a compatible catalog row does not prove an unlisted model safe.
    for catalog in [None, projection.catalog.clone()] {
        let mut large = projection.clone();
        large.catalog = catalog;
        large.settings.model_context_window = Some(500000);
        assert!(projection_for_runtime(&large,&old,&native).unwrap_err().to_string().contains("uncatalogued model"));
        large.catalog = None;
        large.settings.model_context_window = Some(272000);
        assert!(projection_for_runtime(&large,&old,&native).is_ok());
    }
    let mut edited=row.clone();
    edited["slug"]=json!("model-b");
    edited["context_settings"]["preserved"]=json!({});
    projection.catalog=Some(write(json!({"models":[row.clone(),edited]})));
    assert!(projection_for_runtime(&projection,&old,&native).unwrap_err().to_string().contains("model model-b"));
    let mut percent=row.clone();
    percent["context_settings"]["owner"]["auto_compact_percent"]=json!(90);
    projection.catalog=Some(write(json!({"models":[percent]})));
    assert!(projection_for_runtime(&projection,&old,&native).is_err(),"even numerically equivalent percentages require V2");
    projection.catalog=Some(write(json!({"models":[row]})));
    std::fs::write(&path,b"{\"models\":[]}").unwrap();
    assert!(projection_for_runtime(&projection,&old,&native).is_err(),"compatibility never bypasses immutable catalog validation");
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
    let mut projection: Projection = serde_json::from_value(json!({
        "version": 2, "route": "chatgpt_file", "auth": {"path": native.join("unused-auth"),
            "parent_device": 0, "parent_inode": 0, "owner": 0, "account": null, "api_file": null},
        "settings": {}, "catalog": catalog, "requires_job": false
    })).unwrap();
    validate_runtime(&projection, &bundle, &native).unwrap();
    // r290 P13: the independent feature gate also covers an explicitly pinned old server.
    projection.settings.model_context_settings_v1=Some(true);
    let legacy = projection_for_runtime(&projection, &bundle, &native).unwrap();
    assert_eq!(legacy.settings.model_context_settings_v1, Some(false));
    projection.settings.model_auto_compact_percent = Some(75);
    assert!(validate_runtime(&projection,&bundle,&native).unwrap_err().to_string().contains("modelContextSettingsV2"));
    set_context_support(&bundle,true);
    validate_runtime(&projection,&bundle,&native).unwrap();
    projection.settings.model_auto_compact_percent = None;
    set_context_support(&bundle,false);
    projection.settings.model_context_settings_v1=None;
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
