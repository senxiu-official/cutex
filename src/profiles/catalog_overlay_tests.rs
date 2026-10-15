//! Overlay resolution (pure) and the official-profile store flow on a private
//! test home with a fixture native cache (no daemon, no native binary).
use super::*;
use crate::config::global_settings::ConfigValueUpdate;
use crate::profiles::catalog_api::{self, ModelPatch};
use crate::profiles::model_variants::store_tests::fixture;

fn row(id: &str, window: u64) -> Value {
    let mut row = catalog_api::template_row(id);
    row["context_window"] = json!(window);
    row["max_context_window"] = json!(window);
    row["base_instructions"] = json!(format!("{id} instructions"));
    row
}

fn ids(rows: &[Value]) -> Vec<&str> {
    rows.iter().filter_map(|row| row["slug"].as_str()).collect()
}

fn config(id: &str, base: &str, window: u64) -> Entry {
    let mut overrides = Map::new();
    overrides.insert("context_window".into(), json!(window));
    Entry { id: id.into(), request_model: Some(base.into()), hidden: false, overrides }
}

#[test]
fn resolve_applies_overrides_follows_upstream_and_reports_vanished_entries() {
    let mut overrides = Map::new();
    overrides.insert("context_window".into(), json!(300_000));
    overrides.insert("max_context_window".into(), Value::Null);
    let overlay = Overlay {
        entries: vec![
            Entry { id: "a".into(), overrides, ..Entry::default() },
            Entry { id: "b".into(), hidden: true, ..Entry::default() },
            config("A [200K]", "a", 200_000),
            config("Gone [1M]", "gone", 1_000_000),
            Entry { id: "old".into(), overrides: Map::from_iter([("display_name".into(), json!("Old"))]), ..Entry::default() },
        ],
        ..Overlay::default()
    };
    // A model upstream adds later ("c") appears without any edit.
    let upstream = vec![row("a", 1_000_000), row("b", 400_000), row("c", 272_000)];
    let resolved = resolve(&upstream, &overlay);
    assert_eq!(ids(&resolved.rows), ["a", "c", "A [200K]"]);
    assert_eq!(resolved.rows[0]["context_window"], 300_000);
    assert!(resolved.rows[0].get("max_context_window").is_none(), "null override removes the key");
    assert_eq!(resolved.rows[0]["base_instructions"], "a instructions");
    // A configuration follows its live base, then its own overrides.
    assert_eq!(resolved.rows[2]["request_model"], "a");
    assert_eq!(resolved.rows[2]["context_window"], 200_000);
    assert_eq!(resolved.rows[2]["max_context_window"], 1_000_000);
    assert_eq!(resolved.rows[2]["base_instructions"], "a instructions");
    let unavailable: Vec<_> = resolved.unavailable.iter().map(|m| (m["id"].as_str().unwrap(), m["reason"].as_str().unwrap())).collect();
    assert_eq!(unavailable, [("Gone [1M]", "baseModelMissing"), ("old", "modelMissing")]);
    catalog_api::validate_catalog(&json!({ "models": resolved.rows })).unwrap();

    // Upstream refreshes base metadata: the configuration picks it up.
    let mut refreshed = upstream.clone();
    refreshed[0]["base_instructions"] = json!("new a instructions");
    assert_eq!(resolve(&refreshed, &overlay).rows[2]["base_instructions"], "new a instructions");
    // The base returns: its configuration is available again, nothing was lost.
    refreshed.push(row("gone", 1_000_000));
    let back = resolve(&refreshed, &overlay);
    assert!(ids(&back.rows).contains(&"Gone [1M]"));
    assert_eq!(back.unavailable.len(), 1);
}

#[test]
fn diff_stores_only_differences_and_round_trips() {
    let upstream = vec![row("a", 1_000_000), row("b", 400_000), row("c", 272_000)];
    let mut edited = upstream.clone();
    edited[0]["context_window"] = json!(500_000);
    edited[0]["base_instructions"] = json!("not editable: ignored");
    edited.remove(1); // b removed by an edit
    let mut variant = config_row(&upstream[0], "A [285K]", "a");
    variant["context_window"] = json!(285_000);
    edited.push(variant);
    let previous = Overlay { entries: vec![config("Gone", "gone", 1)], ..Overlay::default() };
    let overlay = diff(&upstream, &edited, &previous, true);
    let summary: Vec<_> = overlay.entries.iter().map(|e| (e.id.as_str(), e.hidden, e.overrides.keys().cloned().collect::<Vec<_>>())).collect();
    assert_eq!(summary, [
        ("a", false, vec!["context_window".to_string()]),
        ("A [285K]", false, vec!["context_window".to_string()]),
        ("b", true, vec![]),
        ("Gone", false, vec!["context_window".to_string()]),
    ]);
    let resolved = resolve(&upstream, &overlay);
    assert_eq!(ids(&resolved.rows), ["a", "c", "A [285K]"]);
    assert_eq!(resolved.rows[0]["base_instructions"], "a instructions", "only editable keys are overlaid");
    assert_eq!(diff(&upstream, &resolved.rows, &overlay, true), overlay, "stable under re-diff");
    // Migration does not hide upstream rows a snapshot never had.
    assert!(diff(&upstream, &edited, &Overlay::default(), false).entries.iter().all(|e| !e.hidden));
}

fn patch(base: &str, window: u64) -> ModelPatch {
    ModelPatch {
        request_model: ConfigValueUpdate::Set(base.into()),
        context_window: ConfigValueUpdate::Set(window),
        ..ModelPatch::default()
    }
}

fn write_upstream(native: &std::path::Path, rows: Vec<Value>) {
    std::fs::write(native.join("models_cache.json"), serde_json::to_vec(&json!({ "models": rows })).unwrap()).unwrap();
}

fn astra() -> Value {
    let mut base = catalog_api::template_row("gpt-6-astra");
    base["context_window"] = json!(1_000_000);
    base["max_context_window"] = json!(1_000_000);
    base["base_instructions"] = json!("native instructions survive catalog edits");
    base
}

fn pointer(account: &StoredAccount) -> Option<PathBuf> {
    catalog_api::load(account).unwrap().path
}

#[test]
fn official_edits_store_an_overlay_and_new_upstream_models_appear() {
    let (_home, account, _bundle, native) = fixture();
    assert!(uses_overlay(&account));
    assert!(catalog_api::update_model(&account, "Astra [400K]", &patch("gpt-6-astra", 400_000)).unwrap());
    let overlay = load(&account).unwrap().unwrap();
    assert_eq!(overlay.entries.len(), 1);
    assert_eq!(overlay.entries[0].request_model.as_deref(), Some("gpt-6-astra"));
    assert_eq!(overlay.entries[0].overrides.keys().collect::<Vec<_>>(), ["context_window"]);
    let first = pointer(&account).unwrap();
    let first_bytes = std::fs::read(&first).unwrap();
    // Content-addressed: the file name is the digest a receipt records.
    let digest = crate::agent_management::file_sha256(&first).unwrap();
    assert_eq!(first.file_name().unwrap().to_string_lossy(), format!("models-{}.json", digest.as_str()));

    // Upstream gains a model: the view lists it, the next launch selects a
    // new effective file that includes it, the previous file is untouched.
    let mut newer = astra();
    newer["slug"] = json!("gpt-6.1-astra");
    write_upstream(&native, vec![astra(), newer]);
    let view = catalog_api::models_resource(&account).unwrap();
    let listed: Vec<_> = view["models"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert_eq!(listed, ["gpt-6-astra", "gpt-6.1-astra", "Astra [400K]"]);
    assert_eq!(view["followsUpstream"], true);
    assert_eq!(view["modelsSource"]["kind"], "overlay");
    assert_eq!(view["catalogOverlay"]["entries"], 1);
    refresh_for_launch(&account.name);
    let second = pointer(&account).unwrap();
    assert_ne!(second, first);
    assert_eq!(std::fs::read(&first).unwrap(), first_bytes, "a running runtime's file never changes");
    let catalog: Value = serde_json::from_slice(&std::fs::read(&second).unwrap()).unwrap();
    assert_eq!(ids(catalog["models"].as_array().unwrap()), ["gpt-6-astra", "gpt-6.1-astra", "Astra [400K]"]);
    // Deterministic: an unchanged upstream selects the same file and digest.
    let digest = crate::agent_management::file_sha256(&second).unwrap();
    refresh_for_launch(&account.id);
    assert_eq!(pointer(&account).unwrap(), second);
    assert_eq!(crate::agent_management::file_sha256(&second).unwrap(), digest);
    crate::launch::selected_profile::validate_model(
        &crate::launch::selected_profile::Route::ChatgptFile, "gpt-6.1-astra", None,
        Some(&crate::launch::stock::VerifiedFile { path: second.clone(), sha256: digest }),
    ).unwrap();

    // Base-model edits are overrides too; reset drops them again.
    catalog_api::update_model(&account, "gpt-6-astra", &ModelPatch {
        auto_compact_token_limit: ConfigValueUpdate::Set(700_000), ..ModelPatch::default()
    }).unwrap();
    let base = load(&account).unwrap().unwrap().entries.into_iter().find(|e| e.id == "gpt-6-astra").unwrap();
    assert_eq!(base.overrides.keys().collect::<Vec<_>>(), ["auto_compact_token_limit"]);
    catalog_api::reset_model(&account, "gpt-6-astra").unwrap();
    assert!(load(&account).unwrap().unwrap().entries.iter().all(|e| e.id != "gpt-6-astra"));
    // Removing an upstream entry hides it; editing it shows it again.
    catalog_api::remove_model(&account, "gpt-6.1-astra").unwrap();
    assert!(load(&account).unwrap().unwrap().entries.iter().any(|e| e.id == "gpt-6.1-astra" && e.hidden));
    assert_eq!(catalog_api::models_resource(&account).unwrap()["models"].as_array().unwrap().len(), 2);
    catalog_api::update_model(&account, "gpt-6.1-astra", &ModelPatch {
        context_window: ConfigValueUpdate::Set(500_000), ..ModelPatch::default()
    }).unwrap();
    assert_eq!(catalog_api::models_resource(&account).unwrap()["models"].as_array().unwrap().len(), 3);
    // Removing every difference returns the profile to the plain native list.
    catalog_api::reset_model(&account, "gpt-6.1-astra").unwrap();
    catalog_api::remove_model(&account, "Astra [400K]").unwrap();
    assert_eq!(load(&account).unwrap(), Some(Overlay::default()), "the emptied overlay stays as the migrated marker");
    assert!(pointer(&account).is_none());
    let view = catalog_api::models_resource(&account).unwrap();
    assert_eq!(view["modelsSource"]["kind"], "nativeCache");
    assert_eq!(view["models"].as_array().unwrap().len(), 2);
}

#[test]
fn configurations_whose_base_left_upstream_stay_but_are_unavailable() {
    let (_home, account, _bundle, native) = fixture();
    let mut other = astra();
    other["slug"] = json!("gpt-5.6-sol");
    write_upstream(&native, vec![astra(), other.clone()]);
    catalog_api::update_model(&account, "Astra [400K]", &patch("gpt-6-astra", 400_000)).unwrap();
    catalog_api::update_model(&account, "Sol [200K]", &patch("gpt-5.6-sol", 200_000)).unwrap();
    // gpt-5.6-sol is retired upstream.
    write_upstream(&native, vec![astra()]);
    let view = catalog_api::models_resource(&account).unwrap();
    let listed: Vec<_> = view["models"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert_eq!(listed, ["gpt-6-astra", "Astra [400K]"]);
    assert_eq!(view["unavailableModels"], json!([{
        "id": "Sol [200K]", "requestModel": "gpt-5.6-sol", "displayName": null,
        "reason": "baseModelMissing", "entryDisplayName": null,
    }]));
    // Launch catalog: only resolvable rows (selecting "Sol [200K]" fails closed).
    refresh_for_launch(&account.name);
    let path = pointer(&account).unwrap();
    let catalog: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(ids(catalog["models"].as_array().unwrap()), ["gpt-6-astra", "Astra [400K]"]);
    let file = crate::launch::stock::VerifiedFile { sha256: crate::agent_management::file_sha256(&path).unwrap(), path };
    assert!(crate::launch::selected_profile::validate_model(
        &crate::launch::selected_profile::Route::ChatgptFile, "Sol [200K]", None, Some(&file)).is_err());
    // Other edits keep the orphaned entry.
    catalog_api::update_model(&account, "Astra [400K]", &patch("gpt-6-astra", 410_000)).unwrap();
    assert!(load(&account).unwrap().unwrap().entries.iter().any(|e| e.id == "Sol [200K]"));
    // The base returns: the configuration is back with its own limits.
    write_upstream(&native, vec![astra(), other]);
    let view = catalog_api::models_resource(&account).unwrap();
    let sol = view["models"].as_array().unwrap().iter().find(|m| m["id"] == "Sol [200K]").unwrap();
    assert_eq!(sol["effective"]["contextWindow"], 200_000);
    // An unavailable entry can be deleted.
    write_upstream(&native, vec![astra()]);
    catalog_api::remove_model(&account, "Sol [200K]").unwrap();
    assert!(load(&account).unwrap().unwrap().entries.iter().all(|e| e.id != "Sol [200K]"));
    assert_eq!(catalog_api::models_resource(&account).unwrap()["unavailableModels"], json!([]));
}

#[test]
fn duplicate_copies_the_overlay_not_credentials() {
    let (_home, source, _bundle, native) = fixture();
    catalog_api::update_model(&source, "Astra [400K]", &patch("gpt-6-astra", 400_000)).unwrap();
    let source_files = crate::profiles::materialize::materialized_account_files(&source).unwrap();
    std::fs::write(&source_files.auth_path, br#"{"tokens":{"access_token":"fixture-only"}}"#).unwrap();
    let copy = crate::profiles::lifecycle::duplicate_profile(&source.id, "ChatGPT copy").unwrap();
    let copy_files = crate::profiles::materialize::materialized_account_files(&copy).unwrap();
    assert!(!copy_files.auth_path.exists());
    assert_eq!(load(&copy).unwrap(), load(&source).unwrap());
    crate::profiles::lifecycle::delete_profile(&source.id).unwrap();
    // The copy follows upstream on its own.
    let mut newer = astra();
    newer["slug"] = json!("gpt-6.1-astra");
    write_upstream(&native, vec![astra(), newer]);
    let view = catalog_api::models_resource(&copy).unwrap();
    let listed: Vec<_> = view["models"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert_eq!(listed, ["gpt-6-astra", "gpt-6.1-astra", "Astra [400K]"]);
    refresh_for_launch(&copy.id);
    let path = pointer(&copy).unwrap();
    assert_eq!(path.parent(), Some(copy_files.config_path.parent().unwrap().canonicalize().unwrap().as_path()));
}

#[test]
fn legacy_snapshot_migrates_once_to_an_overlay_with_backup() {
    let (_home, account, _bundle, native) = fixture();
    // A pre-overlay edit: full snapshot with a configuration and an edited base.
    let mut base = astra();
    base["auto_compact_token_limit"] = json!(800_000);
    let mut variant = config_row(&astra(), "Astra [400K]", "gpt-6-astra");
    variant["context_window"] = json!(400_000);
    let mut retired = astra();
    retired["slug"] = json!("gpt-5-retired");
    retired["context_window"] = json!(128_000);
    let dir = catalog_api::profile_dir(&account).unwrap();
    let legacy = dir.join(format!("models-{}.json", uuid::Uuid::new_v4()));
    std::fs::write(&legacy, serde_json::to_vec_pretty(&json!({ "models": [base, retired, variant] })).unwrap()).unwrap();
    let legacy = legacy.canonicalize().unwrap();
    let legacy_bytes = std::fs::read(&legacy).unwrap();
    set_pointer(&account, ConfigValueUpdate::Set(legacy.to_string_lossy().into_owned())).unwrap();
    // Upstream meanwhile gained a model the snapshot never saw.
    let mut newer = astra();
    newer["slug"] = json!("gpt-6.1-astra");
    write_upstream(&native, vec![astra(), newer]);

    let view = catalog_api::models_resource(&account).unwrap();
    let overlay = load(&account).unwrap().expect("migrated");
    assert_eq!(overlay.migrated_from.as_deref(), Some(legacy.to_string_lossy().as_ref()));
    let summary: Vec<_> = overlay.entries.iter().map(|e| (e.id.as_str(), e.request_model.as_deref())).collect();
    assert_eq!(summary, [("gpt-6-astra", None), ("gpt-5-retired", None), ("Astra [400K]", Some("gpt-6-astra"))]);
    assert_eq!(overlay.entries[0].overrides.keys().collect::<Vec<_>>(), ["auto_compact_token_limit"]);
    let listed: Vec<_> = view["models"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert_eq!(listed, ["gpt-6-astra", "gpt-6.1-astra", "Astra [400K]"], "new upstream model is not hidden");
    assert_eq!(view["unavailableModels"][0]["id"], "gpt-5-retired");
    assert_eq!(view["catalogOverlay"]["migratedFrom"], legacy.to_string_lossy().as_ref());
    // Backup of the previous config + snapshot; the snapshot itself is untouched.
    let backup = std::fs::read_dir(&dir).unwrap().filter_map(Result::ok)
        .find(|entry| entry.file_name().to_string_lossy().starts_with("pre-overlay-")).expect("backup").path();
    assert_eq!(std::fs::read(backup.join(legacy.file_name().unwrap())).unwrap(), legacy_bytes);
    assert!(std::fs::read_to_string(backup.join("config.toml")).unwrap().contains(legacy.file_name().unwrap().to_str().unwrap()));
    assert_eq!(std::fs::read(&legacy).unwrap(), legacy_bytes);
    let selected = pointer(&account).unwrap();
    assert_ne!(selected, legacy);
    // Once only.
    assert!(!ensure_migrated(&account).unwrap());
    refresh_for_launch(&account.name);
    assert_eq!(pointer(&account).unwrap(), selected);
}

#[test]
fn api_key_profiles_keep_their_own_catalog() {
    let _home = crate::profiles::lifecycle::tests::TestHome::new();
    let account = crate::profiles::lifecycle::create_profile(&crate::profiles::lifecycle::NewProfile {
        name: "Relay".into(), kind: crate::profiles::lifecycle::NewProfileKind::ApiKey, preset: None,
        base_url: Some("https://relay.example.invalid/v1".into()), model: Some("relay-model".into()),
        api_key: Some(crate::profiles::settings_api::ApiKey("sk-fixture".into())),
    }).unwrap();
    assert!(!uses_overlay(&account));
    assert!(catalog_api::update_model(&account, "relay-model", &ModelPatch {
        context_window: ConfigValueUpdate::Set(100_000), ..ModelPatch::default()
    }).unwrap());
    assert!(load(&account).unwrap().is_none());
    assert!(pointer(&account).unwrap().file_name().unwrap().to_string_lossy().starts_with("models-"));
    assert_eq!(catalog_api::models_resource(&account).unwrap()["followsUpstream"], false);
}

// --- GPT review 2026-10-11, R5-R9 ---------------------------------------

fn sol() -> Value {
    let mut row = astra();
    row["slug"] = json!("gpt-5.6-sol");
    row
}

fn with_window(mut row: Value, window: u64) -> Value {
    row["context_window"] = json!(window);
    row
}

fn listed(account: &StoredAccount) -> Vec<String> {
    catalog_api::models_resource(account).unwrap()["models"].as_array().unwrap()
        .iter().map(|m| m["id"].as_str().unwrap().to_string()).collect()
}

fn effective_window(account: &StoredAccount, id: &str) -> Value {
    let view = catalog_api::models_resource(account).unwrap();
    view["models"].as_array().unwrap().iter().find(|m| m["id"] == id)
        .unwrap_or_else(|| panic!("{id} not listed"))["effective"]["contextWindow"].clone()
}

fn launches(account: &StoredAccount, model: &str) -> bool {
    refresh_for_launch(&account.id);
    let Some(path) = pointer(account) else { return false };
    let file = crate::launch::stock::VerifiedFile { sha256: crate::agent_management::file_sha256(&path).unwrap(), path };
    crate::launch::selected_profile::validate_model(
        &crate::launch::selected_profile::Route::ChatgptFile, model, None, Some(&file)).is_ok()
}

fn select_model(account: &StoredAccount, model: &str) {
    let patch = crate::profiles::settings_api::ProfileApiPatch {
        codex_config: crate::profiles::codex_profile::CodexProfileConfigPatch {
            model: ConfigValueUpdate::Set(model.into()), ..Default::default()
        },
        ..Default::default()
    };
    crate::profiles::settings_api::update_profile(&account.id, &patch).unwrap();
}

/// R5: an edit patches its own entry; other entries (and untouched fields of
/// the edited one) keep pinned values that happen to equal upstream right now.
#[test]
fn edits_keep_pinned_values_that_temporarily_equal_upstream() {
    let (_home, account, _bundle, native) = fixture();
    write_upstream(&native, vec![astra(), sol()]);
    catalog_api::update_model(&account, "Fast", &patch("gpt-6-astra", 400_000)).unwrap();
    catalog_api::update_model(&account, "gpt-5.6-sol", &ModelPatch {
        context_window: ConfigValueUpdate::Set(300_000), ..ModelPatch::default()
    }).unwrap();
    // Upstream happens to move to the pinned values.
    write_upstream(&native, vec![with_window(astra(), 400_000), with_window(sol(), 300_000)]);
    // Unrelated edit, and a display-only edit of Fast itself.
    catalog_api::update_model(&account, "Other", &patch("gpt-6-astra", 200_000)).unwrap();
    catalog_api::update_model(&account, "Fast", &ModelPatch {
        display_name: ConfigValueUpdate::Set("Fast one".into()), ..ModelPatch::default()
    }).unwrap();
    // Upstream changes again: the pinned values stay.
    write_upstream(&native, vec![astra(), sol()]);
    assert_eq!(effective_window(&account, "Fast"), 400_000);
    assert_eq!(effective_window(&account, "gpt-5.6-sol"), 300_000);
    assert_eq!(effective_window(&account, "Other"), 200_000);
    assert_eq!(effective_window(&account, "gpt-6-astra"), 1_000_000);
    // An explicit reset still returns an entry to upstream.
    catalog_api::reset_model(&account, "gpt-5.6-sol").unwrap();
    assert_eq!(effective_window(&account, "gpt-5.6-sol"), 1_000_000);
    assert!(load(&account).unwrap().unwrap().entries.iter().all(|e| e.id != "gpt-5.6-sol"));
}

/// R6: an effective-catalog pointer without an overlay (the overlay of the
/// last entry was removed, the pointer not yet cleared) is never re-migrated.
#[test]
fn emptied_overlay_is_not_remigrated_from_its_effective_file() {
    let (_home, account, _bundle, _native) = fixture();
    catalog_api::update_model(&account, "Fast", &patch("gpt-6-astra", 400_000)).unwrap();
    assert!(pointer(&account).is_some());
    // Crash window of a release that deleted the overlay file first.
    std::fs::remove_file(catalog_api::profile_dir(&account).unwrap().join(OVERLAY_FILE)).unwrap();
    refresh_for_launch(&account.id);
    assert!(listed(&account).iter().all(|id| id != "Fast"), "deleted configuration came back");
    assert!(pointer(&account).is_none());
    // The same crash window of this release: the empty overlay is the marker.
    catalog_api::update_model(&account, "Fast", &patch("gpt-6-astra", 400_000)).unwrap();
    let stale = pointer(&account).unwrap();
    write_private_pretty_json_atomic(
        &catalog_api::profile_dir(&account).unwrap().join(OVERLAY_FILE), &Overlay::default(), "test overlay").unwrap();
    assert_eq!(pointer(&account).unwrap(), stale);
    refresh_for_launch(&account.id);
    assert!(listed(&account).iter().all(|id| id != "Fast"));
    assert!(pointer(&account).is_none());
    assert_eq!(load(&account).unwrap(), Some(Overlay::default()));
}

/// R6: overlay edits take a per-profile file lock (another process).
#[test]
fn overlay_edits_wait_for_the_cross_process_profile_lock() {
    use fs2::FileExt;
    let (_home, account, _bundle, _native) = fixture();
    let dir = catalog_api::profile_dir(&account).unwrap();
    let held = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true)
        .open(dir.join("model-overlay.lock")).unwrap();
    held.lock_exclusive().unwrap();
    let editor = {
        let account = account.clone();
        std::thread::spawn(move || catalog_api::update_model(&account, "Fast", &patch("gpt-6-astra", 400_000)).map(|_| ()))
    };
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(!editor.is_finished(), "edit ran while another process held the profile lock");
    assert!(load(&account).unwrap().is_none());
    FileExt::unlock(&held).unwrap();
    editor.join().unwrap().unwrap();
    assert_eq!(load(&account).unwrap().unwrap().entries.len(), 1);
}

/// R7: picker visibility is display only; a hidden base still resolves.
#[test]
fn configurations_on_a_hidden_base_stay_launchable() {
    let (_home, account, _bundle, native) = fixture();
    write_upstream(&native, vec![astra(), sol()]);
    catalog_api::update_model(&account, "Fast", &patch("gpt-6-astra", 400_000)).unwrap();
    select_model(&account, "Fast");
    let mut hidden = astra();
    hidden["visibility"] = json!("hide");
    write_upstream(&native, vec![hidden, sol()]);
    let view = catalog_api::models_resource(&account).unwrap();
    assert_eq!(view["unavailableModels"], json!([]));
    assert_eq!(listed(&account), ["gpt-5.6-sol", "Fast"], "hidden base is not shown");
    assert!(launches(&account, "Fast"));
    // Really removed upstream: unavailable, launch fails closed.
    write_upstream(&native, vec![sol()]);
    assert_eq!(catalog_api::models_resource(&account).unwrap()["unavailableModels"][0]["id"], "Fast");
    assert!(!launches(&account, "Fast"));
}

/// R8: an unavailable configuration keeps its ID; an upstream model with the
/// same slug does not take over the selection until the entry is deleted.
#[test]
fn unavailable_configurations_keep_their_id_reserved() {
    let (_home, account, _bundle, native) = fixture();
    write_upstream(&native, vec![astra(), sol()]);
    catalog_api::update_model(&account, "gpt-7", &patch("gpt-5.6-sol", 200_000)).unwrap();
    let mut upstream_seven = astra();
    upstream_seven["slug"] = json!("gpt-7");
    write_upstream(&native, vec![astra(), upstream_seven]);
    let view = catalog_api::models_resource(&account).unwrap();
    assert_eq!(view["unavailableModels"][0]["id"], "gpt-7");
    assert_eq!(listed(&account), ["gpt-6-astra"]);
    assert!(!launches(&account, "gpt-7"), "upstream gpt-7 took over the saved configuration");
    assert!(catalog_api::update_model(&account, "gpt-7", &ModelPatch {
        context_window: ConfigValueUpdate::Set(100_000), ..ModelPatch::default()
    }).is_err());
    // Deleting the configuration releases the ID.
    catalog_api::remove_model(&account, "gpt-7").unwrap();
    assert_eq!(listed(&account), ["gpt-6-astra", "gpt-7"]);
}

/// R9: requestModel is immutable against the stored overlay, also for an
/// unavailable configuration.
#[test]
fn unavailable_configurations_cannot_be_rebound() {
    let (_home, account, _bundle, native) = fixture();
    write_upstream(&native, vec![astra(), sol()]);
    catalog_api::update_model(&account, "Fast", &patch("gpt-5.6-sol", 200_000)).unwrap();
    write_upstream(&native, vec![astra()]);
    assert!(catalog_api::update_model(&account, "Fast", &patch("gpt-6-astra", 400_000)).is_err());
    let stored = load(&account).unwrap().unwrap();
    assert_eq!(stored.entries, vec![config("Fast", "gpt-5.6-sol", 200_000)]);
    write_upstream(&native, vec![astra(), sol()]);
    assert_eq!(effective_window(&account, "Fast"), 200_000);
    // Reusing the ID requires deleting it first.
    write_upstream(&native, vec![astra()]);
    catalog_api::remove_model(&account, "Fast").unwrap();
    catalog_api::update_model(&account, "Fast", &patch("gpt-6-astra", 400_000)).unwrap();
    assert_eq!(effective_window(&account, "Fast"), 400_000);
}
