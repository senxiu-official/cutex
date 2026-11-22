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
    Entry { id: id.into(), request_model: Some(base.into()), hidden: false, overrides, ..Entry::default() }
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
    // r290 P13: selecting absolute mode also pins the other mode cleared.
    assert_eq!(base.overrides.keys().collect::<Vec<_>>(), ["auto_compact_percent", "auto_compact_token_limit"]);
    assert_eq!(base.overrides["auto_compact_percent"],Value::Null);
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

// r290 P13
#[test]
fn context_migration_preserves_official_overlay_hidden_and_variant_limits_after_retry() {
    let (_home, account, bundle, native) = fixture();
    write_upstream(&native,vec![astra(),row("hidden",300000)]);
    catalog_api::update_model(&account,"Astra [400K]",&patch("gpt-6-astra",400000)).unwrap();
    catalog_api::remove_model(&account,"hidden").unwrap();
    let old_path=pointer(&account).unwrap();
    let old_bytes=std::fs::read(&old_path).unwrap();
    settings_api::update_profile(&account.id,&ProfileApiPatch {codex_config: CodexProfileConfigPatch {
        model_context_window:ConfigValueUpdate::Set(200000),
        model_auto_compact_token_limit:ConfigValueUpdate::Set(150000),..Default::default()
    },..Default::default()}).unwrap();
    crate::profiles::model_variants::store_tests::set_context_support(&bundle,true);
    let before=catalog_api::load(&account).unwrap();
    let before_rows=before.catalog.unwrap()["models"].as_array().unwrap().clone();
    FAIL_CONTEXT_COMMIT.with(|fail|fail.set(true));
    assert!(ensure_context_migrated(&account).is_err());
    assert!(!settings_api::codex_snapshot(&account).model_context_settings_v1);
    assert_eq!(pointer(&account).as_ref(),Some(&old_path));
    assert!(ensure_context_migrated(&account).unwrap());
    assert!(!ensure_context_migrated(&account).unwrap());
    let overlay=load(&account).unwrap().unwrap();
    let hidden=overlay.entries.iter().find(|entry|entry.id=="hidden").unwrap();
    assert!(hidden.hidden);
    assert!(hidden.overrides.is_empty());
    assert!(hidden.context_preserved.is_empty());
    let variant=overlay.entries.iter().find(|entry|entry.id=="Astra [400K]").unwrap();
    assert_eq!(variant.overrides["context_window"],400000);
    assert_eq!(variant.context_preserved["context_window"],200000);
    let after=catalog_api::load(&account).unwrap();
    for old in &before_rows {
        let new=after.catalog.as_ref().unwrap()["models"].as_array().unwrap().iter()
            .find(|r|r["slug"]==old["slug"]).unwrap();
        let previous=super::super::model_context::resolve(old,&before.snapshot);
        let current=super::super::model_context::resolve(new,&after.snapshot);
        assert_eq!((previous.window,previous.absolute),(current.window,current.absolute));
    }
    assert_eq!(std::fs::read(&old_path).unwrap(),old_bytes);
    let mut refreshed=astra(); refreshed["context_window"]=json!(900000);
    write_upstream(&native,vec![refreshed]);
    refresh_for_launch(&account.id);
    let current=catalog_api::models_resource(&account).unwrap();
    for row in current["models"].as_array().unwrap() {
        assert_eq!(row["effective"]["contextWindow"],200000);
        assert_eq!(row["effective"]["autoCompactTokenLimit"],150000);
        assert_eq!(row["effective"]["contextWindowSource"],if row["id"]=="Astra [400K]" {"preserved"}else{"profile"});
    }
}

#[test]
fn context_migration_custom_catalog_is_immutable_and_unavailable_catalog_stays_legacy() {
    let (_home, account, bundle, _native)=fixture();
    // Direct custom catalog path, even on an official profile, stays a snapshot.
    let mut base=astra(); base["context_window"]=json!(128000); base["max_context_window"]=json!(256000);
    let dir=catalog_api::profile_dir(&account).unwrap();
    let original=dir.join(format!("models-{}.json",uuid::Uuid::new_v4()));
    write_private_pretty_json_atomic(&original,&json!({"models":[base]}),"fixture").unwrap();
    settings_api::update_profile(&account.id,&ProfileApiPatch {codex_config: CodexProfileConfigPatch {
        model_catalog_json:ConfigValueUpdate::Set(original.to_string_lossy().into_owned()),
        model_context_window:ConfigValueUpdate::Set(500000),
        model_auto_compact_token_limit:ConfigValueUpdate::Set(400000),..Default::default()
    },..Default::default()}).unwrap();
    let original_bytes=std::fs::read(&original).unwrap();
    crate::profiles::model_variants::store_tests::set_context_support(&bundle,true);
    assert!(ensure_context_migrated(&account).unwrap());
    assert!(load(&account).unwrap().is_none());
    let state=catalog_api::load(&account).unwrap();
    let value=super::super::model_context::resolve(&state.catalog.unwrap()["models"][0],&state.snapshot);
    assert_eq!((value.window,value.absolute),(Some(256000),Some(230400)));
    assert_eq!(std::fs::read(&original).unwrap(),original_bytes);
    refresh_for_launch(&account.id);
    let resource=catalog_api::models_resource(&account).unwrap();
    assert_eq!(resource["catalogPresent"],true);
    assert_eq!(resource["followsUpstream"],false);
    assert!(load(&account).unwrap().is_none(),"unknown legacy snapshot must not be inferred as owner edits");
    assert_eq!(resource["models"][0]["contextSettings"]["owner"]["contextWindow"],Value::Null);
    use crate::profiles::lifecycle::{create_profile,NewProfile,NewProfileKind};
    let missing=create_profile(&NewProfile {name:"missing catalog".into(),kind:NewProfileKind::ApiKey,
        preset:None,base_url:Some("https://example.invalid/v1".into()),model:None,api_key:None}).unwrap();
    settings_api::update_profile(&missing.id,&ProfileApiPatch {codex_config:CodexProfileConfigPatch {
        model_catalog_json:ConfigValueUpdate::Set(dir.join("absent.json").to_string_lossy().into_owned()),..Default::default()
    },..Default::default()}).unwrap();
    assert!(ensure_context_migrated(&missing).is_err());
    assert!(!settings_api::codex_snapshot(&missing).model_context_settings_v1);
}

// r290 P13
#[test]
fn context_migration_preserves_legacy_profile_absolute_above_its_window_and_model_edits() {
    let (_home,account,bundle,_native)=fixture();
    let files=crate::profiles::materialize::materialized_account_files(&account).unwrap();
    let raw=std::fs::read_to_string(&files.config_path).unwrap();
    let mut config:toml::Table=toml::from_str(&raw).unwrap();
    config.insert("model_context_window".into(),toml::Value::Integer(200000));
    config.insert("model_auto_compact_token_limit".into(),toml::Value::Integer(500000));
    std::fs::write(&files.config_path,toml::to_string(&config).unwrap()).unwrap();
    crate::profiles::model_variants::store_tests::set_context_support(&bundle,true);
    assert!(ensure_context_migrated(&account).unwrap());
    let state=catalog_api::load(&account).unwrap();
    assert_eq!(state.snapshot.model_auto_compact_token_limit,Some(500000));
    assert!(state.catalog.is_none(),"ordinary official migration must not select a catalog");
    let current=catalog_api::models_resource(&account).unwrap()["models"][0]["effective"].clone();
    assert_eq!((current["contextWindow"].as_i64(),current["autoCompactTokenLimit"].as_i64()),(Some(200000),Some(180000)));
    // Editing a model's catalog must not revalidate an unchanged legacy default.
    catalog_api::update_model(&account,"gpt-6-astra",&ModelPatch {
        auto_compact_percent:ConfigValueUpdate::Set(50),..Default::default()
    }).unwrap();
    let state=catalog_api::load(&account).unwrap();
    assert_eq!(state.snapshot.model_auto_compact_token_limit,Some(500000));
    let current=super::super::model_context::resolve(&state.catalog.unwrap()["models"][0],&state.snapshot);
    assert_eq!((current.window,current.absolute),(Some(200000),Some(100000)));
}

// r290 P13
#[test]
fn context_first_custom_model_can_create_a_catalog_after_automatic_migration_defers() {
    let (_home,_official,bundle,_native)=fixture();
    use crate::profiles::lifecycle::{create_profile,NewProfile,NewProfileKind};
    let account=create_profile(&NewProfile {name:"first custom model".into(),kind:NewProfileKind::ApiKey,
        preset:None,base_url:Some("https://example.invalid/v1".into()),model:None,api_key:None}).unwrap();
    settings_api::update_profile(&account.id,&ProfileApiPatch {codex_config:CodexProfileConfigPatch {
        model_context_window:ConfigValueUpdate::Set(200000),
        model_auto_compact_token_limit:ConfigValueUpdate::Set(150000),..Default::default()
    },..Default::default()}).unwrap();
    assert!(catalog_api::load(&account).unwrap().catalog.is_none());
    crate::profiles::model_variants::store_tests::set_context_support(&bundle,true);
    assert!(ensure_context_migrated(&account).is_err());
    assert!(!settings_api::codex_snapshot(&account).model_context_settings_v1);
    assert!(catalog_api::update_model(&account,"vendor/new",&ModelPatch {
        context_window:ConfigValueUpdate::Set(128000),auto_compact_percent:ConfigValueUpdate::Set(50),..Default::default()
    }).unwrap());
    let state=catalog_api::load(&account).unwrap();
    assert!(state.snapshot.model_context_settings_v1);
    assert_eq!(state.snapshot.model_context_window,Some(200000));
    assert_eq!(state.snapshot.model_auto_compact_token_limit,Some(150000));
    let model=super::super::model_context::resolve(&state.catalog.unwrap()["models"][0],&state.snapshot);
    assert_eq!((model.window,model.absolute),(Some(128000),Some(64000)));
    assert_eq!(model.compact_source,"model");
    assert!(catalog_api::update_model(&account,"vendor/window-only",&ModelPatch {
        context_window:ConfigValueUpdate::Set(128000),..Default::default()
    }).unwrap());
    let state=catalog_api::load(&account).unwrap();
    let catalog=state.catalog.unwrap();
    let row=catalog["models"].as_array().unwrap().iter().find(|row|row["slug"]=="vendor/window-only").unwrap();
    let model=super::super::model_context::resolve(row,&state.snapshot);
    assert_eq!((model.window,model.absolute),(Some(128000),Some(115200)));
    assert_eq!(model.compact_source,"profile");
    assert!(!ensure_context_migrated(&account).unwrap());
}

// r290 P13: persistence must retain the same per-field inheritance as the pure resolver.
#[test]
fn context_migration_field_pins_preserve_official_and_custom_inheritance() {
    for official in [true,false] {
        for window_only in [true,false] {
            let (_home,base,bundle,native)=fixture();
            let mut model=astra();model["context_window"]=json!(128000);model["max_context_window"]=json!(256000);
            let account=if official {write_upstream(&native,vec![model]);base}else{
                use crate::profiles::lifecycle::{create_profile,NewProfile,NewProfileKind};
                let account=create_profile(&NewProfile {name:"inheritance custom".into(),kind:NewProfileKind::ApiKey,preset:None,base_url:Some("https://example.invalid/v1".into()),model:None,api_key:None}).unwrap();
                let file=catalog_api::profile_dir(&account).unwrap().join("custom-inheritance.json");
                write_private_pretty_json_atomic(&file,&json!({"models":[model]}),"fixture").unwrap();
                settings_api::update_profile(&account.id,&ProfileApiPatch {codex_config:CodexProfileConfigPatch {model_catalog_json:ConfigValueUpdate::Set(file.to_string_lossy().into_owned()),..Default::default()},..Default::default()}).unwrap();account
            };
            let old=if window_only {CodexProfileConfigPatch {model_context_window:ConfigValueUpdate::Set(200000),..Default::default()}}else{CodexProfileConfigPatch {model_auto_compact_token_limit:ConfigValueUpdate::Set(150000),..Default::default()}};
            settings_api::update_profile(&account.id,&ProfileApiPatch {codex_config:old,..Default::default()}).unwrap();
            crate::profiles::model_variants::store_tests::set_context_support(&bundle,true);
            assert!(ensure_context_migrated(&account).unwrap());
            if official {assert!(pointer(&account).is_none());assert!(load(&account).unwrap().unwrap().entries.is_empty());}
            else {assert!(catalog_api::load_edits(&account).is_none());}
            let new=CodexProfileConfigPatch {model_context_window:ConfigValueUpdate::Set(240000),model_auto_compact_percent:ConfigValueUpdate::Set(75),..Default::default()};
            settings_api::update_profile(&account.id,&ProfileApiPatch {codex_config:new,..Default::default()}).unwrap();
            let view=catalog_api::models_resource(&account).unwrap();let limits=&view["models"][0]["effective"];
            assert_eq!(limits["contextWindow"],240000);assert_eq!(limits["autoCompactTokenLimit"],180000);
            assert_eq!(limits["contextWindowSource"],"profile");assert_eq!(limits["autoCompactTokenLimitSource"],"profile");
            assert!(!ensure_context_migrated(&account).unwrap());
        }
    }
}

// r290 P13: prove stored provenance and one-click restore for both catalog kinds.
#[test]
fn context_owner_conflict_restore_and_explicit_edit_keep_originals_in_both_stores() {
    for official in [true,false] {
        for restore in [true,false] {
            let (_home,base,bundle,native)=fixture();
            let mut model=astra();model["context_window"]=json!(256000);model["max_context_window"]=json!(256000);
            let account=if official {write_upstream(&native,vec![model]);base}else{
                use crate::profiles::lifecycle::{create_profile,NewProfile,NewProfileKind};
                let a=create_profile(&NewProfile{name:"owner conflict".into(),kind:NewProfileKind::ApiKey,preset:None,base_url:Some("https://example.invalid/v1".into()),model:None,api_key:None}).unwrap();
                let path=catalog_api::profile_dir(&a).unwrap().join("owner-base.json");
                write_private_pretty_json_atomic(&path,&json!({"models":[model]}),"fixture").unwrap();
                settings_api::update_profile(&a.id,&ProfileApiPatch{codex_config:CodexProfileConfigPatch{model_catalog_json:ConfigValueUpdate::Set(path.to_string_lossy().into_owned()),..Default::default()},..Default::default()}).unwrap();a
            };
            catalog_api::update_model(&account,"gpt-6-astra",&ModelPatch{context_window:ConfigValueUpdate::Set(128000),auto_compact_token_limit:ConfigValueUpdate::Set(90000),..Default::default()}).unwrap();
            settings_api::update_profile(&account.id,&ProfileApiPatch{codex_config:CodexProfileConfigPatch{model_context_window:ConfigValueUpdate::Set(200000),model_auto_compact_token_limit:ConfigValueUpdate::Set(150000),..Default::default()},..Default::default()}).unwrap();
            let old=catalog_api::load(&account).unwrap();let old_path=old.path.unwrap();let old_bytes=std::fs::read(&old_path).unwrap();
            crate::profiles::model_variants::store_tests::set_context_support(&bundle,true);
            assert!(ensure_context_migrated(&account).unwrap());
            let stored=catalog_api::load(&account).unwrap().catalog.unwrap();
            assert!(stored["models"][0]["context_settings"]["catalog"].is_object(),"Native must always receive the catalog layer");
            let view=catalog_api::models_resource(&account).unwrap();let row=&view["models"][0];
            assert_eq!(row["effective"]["contextWindow"],200000);assert_eq!(row["effective"]["autoCompactTokenLimit"],150000);
            assert_eq!(row["effective"]["contextWindowSource"],"preserved");assert_eq!(row["effective"]["autoCompactTokenLimitSource"],"preserved");
            assert_eq!(row["contextSettings"]["owner"]["contextWindow"],128000);assert_eq!(row["contextSettings"]["owner"]["autoCompactTokenLimit"],90000);
            assert_eq!(std::fs::read(old_path).unwrap(),old_bytes);
            if restore {
                catalog_api::update_model(&account,"gpt-6-astra",&ModelPatch{restore_preserved_context_window:true,restore_preserved_auto_compaction:true,..Default::default()}).unwrap();
                let view=catalog_api::models_resource(&account).unwrap();let row=&view["models"][0];
                assert_eq!(row["effective"]["contextWindow"],128000);assert_eq!(row["effective"]["autoCompactTokenLimit"],90000);
                assert_eq!(row["effective"]["contextWindowSource"],"model");assert_eq!(row["effective"]["autoCompactTokenLimitSource"],"model");
            }else{
                catalog_api::update_model(&account,"gpt-6-astra",&ModelPatch{auto_compact_percent:ConfigValueUpdate::Set(50),..Default::default()}).unwrap();
                let view=catalog_api::models_resource(&account).unwrap();let row=&view["models"][0];
                assert_eq!(row["effective"]["contextWindowSource"],"preserved");assert_eq!(row["effective"]["autoCompactTokenLimit"],100000);
                assert_eq!(row["effective"]["autoCompactTokenLimitSource"],"model");
                catalog_api::update_model(&account,"gpt-6-astra",&ModelPatch{context_window:ConfigValueUpdate::Set(160000),..Default::default()}).unwrap();
                let view=catalog_api::models_resource(&account).unwrap();let row=&view["models"][0];
                assert_eq!(row["effective"]["contextWindow"],160000);assert_eq!(row["effective"]["autoCompactTokenLimit"],80000);
                assert_eq!(row["effective"]["contextWindowSource"],"model");
            }
            catalog_api::update_model(&account,"gpt-6-astra",&ModelPatch{context_window:ConfigValueUpdate::Clear,..Default::default()}).unwrap();
            let defaults=catalog_api::models_resource(&account).unwrap();
            assert_eq!(defaults["models"][0]["effective"]["contextWindow"],200000);
            assert_eq!(defaults["models"][0]["effective"]["contextWindowSource"],"profile");
            catalog_api::reset_model(&account,"gpt-6-astra").unwrap();
            let reset=catalog_api::models_resource(&account).unwrap();
            assert_eq!(reset["models"][0]["contextSettings"]["owner"]["contextWindow"],Value::Null);
            assert_eq!(reset["models"][0]["effective"]["contextWindowSource"],"profile");
            assert_eq!(reset["models"][0]["effective"]["autoCompactTokenLimitSource"],"profile");
            assert!(!ensure_context_migrated(&account).unwrap());
        }
    }
}

// r290 P13: B validates owner absolute choices against their real inherited window.
#[test]
fn context_b_edits_validate_absolute_against_inherited_window_and_cap_untouched_defaults() {
    for official in [true,false] {
        let (_home,base,bundle,native)=fixture();
        let mut model=astra();model["context_window"]=json!(128000);model["max_context_window"]=json!(256000);model["auto_compact_token_limit"]=json!(90000);
        let account=if official {write_upstream(&native,vec![model]);base}else{
            use crate::profiles::lifecycle::{create_profile,NewProfile,NewProfileKind};
            let a=create_profile(&NewProfile{name:"B validation".into(),kind:NewProfileKind::ApiKey,preset:None,base_url:Some("https://example.invalid/v1".into()),model:None,api_key:None}).unwrap();
            let path=catalog_api::profile_dir(&a).unwrap().join("validation-base.json");
            write_private_pretty_json_atomic(&path,&json!({"models":[model]}),"fixture").unwrap();
            settings_api::update_profile(&a.id,&ProfileApiPatch{codex_config:CodexProfileConfigPatch{model_catalog_json:ConfigValueUpdate::Set(path.to_string_lossy().into_owned()),..Default::default()},..Default::default()}).unwrap();a
        };
        settings_api::update_profile(&account.id,&ProfileApiPatch{codex_config:CodexProfileConfigPatch{model_context_window:ConfigValueUpdate::Set(200000),..Default::default()},..Default::default()}).unwrap();
        crate::profiles::model_variants::store_tests::set_context_support(&bundle,true);ensure_context_migrated(&account).unwrap();
        catalog_api::update_model(&account,"gpt-6-astra",&ModelPatch{auto_compact_token_limit:ConfigValueUpdate::Set(150000),..Default::default()}).unwrap();
        let view=catalog_api::models_resource(&account).unwrap();assert_eq!(view["models"][0]["effective"]["autoCompactTokenLimit"],150000);
        catalog_api::update_model(&account,"gpt-6-astra",&ModelPatch{context_window:ConfigValueUpdate::Set(64000),..Default::default()}).unwrap();
        catalog_api::update_model(&account,"gpt-6-astra",&ModelPatch{display_name:ConfigValueUpdate::Set("small window".into()),..Default::default()}).unwrap();
        let view=catalog_api::models_resource(&account).unwrap();assert_eq!(view["models"][0]["effective"]["autoCompactTokenLimit"],57600);
        assert!(catalog_api::update_model(&account,"gpt-6-astra",&ModelPatch{auto_compact_token_limit:ConfigValueUpdate::Set(70000),..Default::default()}).is_err());
        catalog_api::update_model(&account,"gpt-6-astra",&ModelPatch{context_window:ConfigValueUpdate::Clear,..Default::default()}).unwrap();
        catalog_api::update_model(&account,"gpt-6-astra",&ModelPatch{auto_compact_token_limit:ConfigValueUpdate::Set(170000),..Default::default()}).unwrap();
        let view=catalog_api::models_resource(&account).unwrap();assert_eq!(view["models"][0]["effective"]["contextWindow"],200000);assert_eq!(view["models"][0]["effective"]["autoCompactTokenLimit"],170000);
        assert!(catalog_api::update_model(&account,"gpt-6-astra",&ModelPatch{auto_compact_token_limit:ConfigValueUpdate::Set(210000),..Default::default()}).is_err());
    }
}
