use super::*;
use crate::management::v2::profiles_api;
use crate::profiles::lifecycle::tests::TestHome;
use crate::profiles::lifecycle::{create_profile, NewProfile, NewProfileKind};
use crate::profiles::settings_api::{self, ApiKey};

const FAKE_DSH: &str = "sk-fake-r265-redaction-dsh-0123456789";
const FAKE_DS_CODEX: &str = "sk-fake-r265-redaction-codex-9876543210";
const FAKE_CLAUDE: &str = "sk-fake-r265-redaction-claude-5555";

fn api_profile(name: &str, preset: &str, key: Option<&str>, base_url: Option<&str>) -> StoredAccount {
    create_profile(&NewProfile {
        name: name.into(),
        kind: NewProfileKind::ApiKey,
        preset: crate::profiles::providers::find_preset(preset),
        base_url: base_url.map(Into::into),
        model: None,
        api_key: key.map(|key| ApiKey(key.into())),
    })
    .unwrap()
}

fn account(id: &str) -> StoredAccount {
    let store = settings_api::load_store().unwrap();
    settings_api::find(&store, id).unwrap().clone()
}

fn bound_session(session_id: &str, account: &StoredAccount, variant: &str) {
    let mut record = crate::session::model::CutexSessionRecord::new(session_id.into(), None, "h".into(), "/tmp".into(), None).unwrap();
    record.profile = Some(account.name.clone());
    let plan = LaunchPlan { account: account.clone(), variant: variants(account).into_iter().find(|v| v.id == variant).unwrap() };
    record.profile_binding = Some(plan.binding(Some("m".into()), None, Some("0.2.1-rc.3".into())));
    crate::session::store::with_locked_session_rows(&crate::session::store::cutex_sessions_path().unwrap(), |rows| {
        rows.insert_session(session_id.to_string(), record);
        rows.commit().map(drop)
    })
    .unwrap();
}

fn compat(item: &Value, runtime: &str) -> (String, Value) {
    let row = item["runtimeCompatibility"].as_array().unwrap().iter().find(|row| row["runtime"] == runtime).unwrap();
    (row["status"].as_str().unwrap().to_string(), row["reason"].clone())
}

#[test]
fn legacy_profiles_project_idempotently_without_rewriting_the_store() {
    let _home = TestHome::new();
    let chatgpt = create_profile(&NewProfile { name: "GPT".into(), kind: NewProfileKind::Chatgpt, preset: None, base_url: None, model: None, api_key: None }).unwrap();
    let glm = api_profile("GLM", "zai", Some("sk-test-only"), None);
    let deepseek = api_profile("DS", "deepseek", Some(FAKE_DS_CODEX), None);
    let relay = api_profile("DS relay", "deepseek", Some("sk-test-only"), Some("https://relay.example/v1"));
    let dsh = profiles_api::create_from_body(br#"{"name":"DSH env","kind":"dsh"}"#).unwrap();
    let claude = create_profile(&NewProfile { name: "Claude".into(), kind: NewProfileKind::Claude, preset: None, base_url: None, model: None, api_key: None }).unwrap();
    let before = std::fs::read(crate::config::paths::accounts_path().unwrap()).unwrap();
    let first = profiles_api::list_resource().unwrap();
    let second = profiles_api::list_resource().unwrap();
    assert_eq!(first["profiles"], second["profiles"], "projection is repeatable");
    assert_eq!(before, std::fs::read(crate::config::paths::accounts_path().unwrap()).unwrap(), "listing never rewrites accounts.json");
    let item = |id: &str| first["profiles"].as_array().unwrap().iter().find(|row| row["id"] == id).unwrap().clone();

    let gpt = item(&chatgpt.id);
    assert_eq!(gpt["credential"]["source"], "chatgptLogin");
    assert_eq!(gpt["runtimeVariants"].as_array().unwrap().len(), 1);
    assert_eq!(gpt["runtimeVariants"][0]["id"], "codex");
    assert_eq!(gpt["defaultVariant"], "codex");
    assert_eq!(compat(&gpt, "dsh").1, "chatgptLoginCodexOnly");

    let glm = item(&glm.id);
    assert_eq!(glm["credential"]["source"], "stored");
    assert_eq!(glm["credential"]["present"], true);
    assert_eq!(glm["providerInterfaces"][0]["kind"], "openaiCompatible");
    assert_eq!(compat(&glm, "dsh"), ("unsupported".into(), json!("providerNotDeepseekNative")));

    let ds = item(&deepseek.id);
    assert!(ds["providerInterfaces"].as_array().unwrap().iter().any(|row| row["kind"] == "deepseekNative"));
    assert_ne!(compat(&ds, "dsh").0, "unsupported");
    assert_eq!(compat(&ds, "claude").1, "anthropicRouteNotYetSupported");
    assert_eq!(ds["runtimeVariants"][0]["defaults"]["model"], ds["model"]);

    // profile-relay-runtimes (2026-10-28): a relay keeps the provider's
    // runtimes; only its DSH address must be set before DSH launches.
    let relay_item = item(&relay.id);
    assert_ne!(compat(&relay_item, "dsh").0, "unsupported", "a DeepSeek key behind a relay keeps DSH");
    assert_eq!(compat(&relay_item, "claude").1, "anthropicRouteNotYetSupported");
    assert_eq!(add_variant(&relay.id, "dsh", None, Default::default()).unwrap_err().code, "variant_base_url_missing");

    let dsh = item(dsh["profile"]["id"].as_str().unwrap());
    assert_eq!(dsh["credential"]["source"], "env");
    assert_eq!(dsh["credential"]["envName"], "DEEPSEEK_API_KEY");
    assert_eq!(dsh["runtimeVariants"][0]["id"], "dsh");
    assert_eq!(compat(&dsh, "codex").1, "dshProfileCodexVariantNotYetSupported");
    assert_eq!(dsh["authKind"], "env", "legacy DSH auth kind unchanged");

    let claude = item(&claude.id);
    assert_eq!(claude["credential"]["source"], "claudeAccount");
    assert_eq!(claude["credential"]["claudeAccountId"], "default");
    assert_eq!(compat(&claude, "codex").1, "claudeCredentialClaudeOnly");

    // Legacy accounts serialize without any r265 field.
    for account in settings_api::load_store().unwrap().accounts {
        let wire = serde_json::to_value(&account).unwrap();
        for key in ["runtime_variants", "default_variant", "credential_source", "credential_version"] {
            assert!(wire.get(key).is_none() || account.cli_kind == CliKind::Codex && key == "credential_version", "{key} on {wire}");
        }
    }
}

/// profile-relay-runtimes (B1, B5): DSH availability follows the provider
/// id; a relay main address needs the DSH address, given with the variant or
/// on the profile, and launches use it.
#[test]
fn deepseek_relay_profiles_keep_dsh_and_require_its_address() {
    use crate::profiles::interface_urls::{require, InterfaceKind, BASE_URL_MISSING, DEEPSEEK_ANTHROPIC_BASE_URL};
    let _home = TestHome::new();
    let relay = api_profile("DS relay", "deepseek", Some(FAKE_DS_CODEX), Some("https://relay.example/v1"));
    assert!(is_deepseek_api_key(&relay));
    assert_ne!(compatibility(&relay, &CliKind::Dsh).0, Status::Unsupported);
    // Presets: the provider decides, never the host.
    let preview = preset_compatibility_view("deepseek", None);
    assert_ne!(preview[2]["status"], "unsupported", "{preview}");
    assert_eq!(preset_compatibility_view("zai", Some("https://api.deepseek.com/"))[2]["reason"], "providerNotDeepseekNative");

    // Missing address: refused when adding the variant and when planning a launch.
    let refused = add_variant(&relay.id, "dsh", None, Default::default()).unwrap_err();
    assert_eq!((refused.status, refused.code), (409, BASE_URL_MISSING));
    assert_eq!(refused.details["interface"], "deepseekNative");
    assert!(account(&relay.id).variants.runtime_variants.is_empty(), "nothing is stored on refusal");
    // A bad address is a 400, not stored.
    let bad = add_variant_with_address(&relay.id, "dsh", None, Default::default(), Some("https://u:p@relay.example".into())).unwrap_err();
    assert_eq!(bad.code, "invalid_request");
    // The address with the variant: stored as the profile's deepseekNative address.
    let added = add_variant_with_address(&relay.id, "dsh", None, Default::default(), Some("https://relay.example/anthropic".into())).unwrap();
    assert_eq!(added.interface_base_urls.get("deepseekNative").map(String::as_str), Some("https://relay.example/anthropic"));
    assert_eq!(require(&added, InterfaceKind::DeepseekNative), Ok(Some("https://relay.example/anthropic".into())));
    require_address(&added, &CliKind::Dsh, "dsh").unwrap();
    // Cleared again (PATCH interfaceBaseUrls null): launches refuse, never the official host.
    let patch = profiles_api::parse_patch(br#"{"interfaceBaseUrls":{"deepseekNative":null}}"#).unwrap();
    settings_api::update_profile(&relay.id, &patch).unwrap();
    let cleared = account(&relay.id);
    assert_eq!(require_address(&cleared, &CliKind::Dsh, "dsh").unwrap_err().code, BASE_URL_MISSING);
    if crate::components::dsh_runtime::advertised() {
        assert_eq!(plan(&cleared, Some("dsh")).unwrap_err().code, BASE_URL_MISSING);
    }
    // The cute-codex primary is unaffected by the DSH address.
    plan(&cleared, Some("codex")).unwrap();

    // Official main address: the DSH address defaults to DeepSeek's Anthropic root.
    let official = api_profile("DS", "deepseek", Some(FAKE_DS_CODEX), None);
    let added = add_variant(&official.id, "dsh", None, Default::default()).unwrap();
    assert_eq!(require(&added, InterfaceKind::DeepseekNative), Ok(Some(DEEPSEEK_ANTHROPIC_BASE_URL.into())));
    // GLM (provider zai) never gets DSH, whatever its address.
    let glm = api_profile("GLM relay", "zai", Some("sk-test-only"), Some("https://api.deepseek.com/"));
    assert_eq!(compatibility(&glm, &CliKind::Dsh), (Status::Unsupported, Some("providerNotDeepseekNative")));
    assert_eq!(add_variant_with_address(&glm.id, "dsh", None, Default::default(), Some("https://relay.example/anthropic".into())).unwrap_err().code,
        "variant_runtime_unsupported");
}

#[test]
fn dsh_variant_on_deepseek_profile_add_default_delete_rules() {
    let _home = TestHome::new();
    let ds = api_profile("DS", "deepseek", Some(FAKE_DS_CODEX), None);
    let glm = api_profile("GLM", "zai", Some("sk-test-only"), None);
    assert_eq!(add_variant(&glm.id, "dsh", None, Default::default()).unwrap_err().code, "variant_runtime_unsupported");
    assert_eq!(add_variant(&ds.id, "claude", None, Default::default()).unwrap_err().code, "variant_runtime_unsupported");
    assert_eq!(add_variant(&ds.id, "dsh", Some("Bad Id"), Default::default()).unwrap_err().code, "invalid_request");
    let bad = VariantDefaults { model: None, reasoning_effort: Some("medium".into()) };
    assert_eq!(add_variant(&ds.id, "dsh", None, bad).unwrap_err().code, "invalid_request");
    let added = add_variant(&ds.id, "dsh", None, VariantDefaults { model: Some("deepseek-v4-pro".into()), reasoning_effort: Some("max".into()) }).unwrap();
    assert_eq!(added.cli_kind, CliKind::Codex, "the primary runtime never changes");
    assert_eq!(add_variant(&ds.id, "dsh", Some("dsh2"), Default::default()).unwrap_err().code, "variant_exists");
    let all = variants(&account(&ds.id));
    assert_eq!(all.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(), ["codex", "dsh"]);

    // Default variant: settable, resets to the primary.
    let patch = profiles_api::parse_patch(br#"{"defaultVariant":"dsh"}"#).unwrap();
    settings_api::update_profile(&ds.id, &patch).unwrap();
    assert_eq!(default_variant_id(&account(&ds.id)), "dsh");
    assert_eq!(require_primary_default(&account(&ds.id)).unwrap_err().code, "variant_runtime_unsupported_here");
    assert!(settings_api::update_profile(&ds.id, &profiles_api::parse_patch(br#"{"defaultVariant":"nope"}"#).unwrap()).is_err());
    settings_api::update_profile(&ds.id, &profiles_api::parse_patch(br#"{"defaultVariant":null}"#).unwrap()).unwrap();
    assert_eq!(default_variant_id(&account(&ds.id)), "codex");

    // Edit secondary defaults.
    use crate::config::global_settings::ConfigValueUpdate as U;
    let edited = patch_variant(&ds.id, "dsh", U::Set("deepseek-v4-flash".into()), U::Clear).unwrap();
    assert_eq!(edited.variants.runtime_variants[0].defaults, VariantDefaults { model: Some("deepseek-v4-flash".into()), reasoning_effort: None });
    assert_eq!(patch_variant(&ds.id, "codex", U::Unchanged, U::Unchanged).unwrap_err().code, "unknown_variant");
    // Defaults follow DSH's pinned catalog (the legacy default above stays accepted).
    let refused = patch_variant(&ds.id, "dsh", U::Set("deepseek-chat".into()), U::Unchanged).unwrap_err();
    assert!(refused.code == "invalid_request" && refused.message.contains("deepseek-flash"), "{refused}");
    let item = profiles_api::list_resource().unwrap()["profiles"].as_array().unwrap().iter().find(|row| row["id"] == ds.id.as_str()).unwrap().clone();
    let dsh = item["runtimeVariants"].as_array().unwrap().iter().find(|row| row["id"] == "dsh").unwrap();
    let ids = dsh["models"].as_array().unwrap().iter().map(|row| row["id"].as_str().unwrap()).collect::<Vec<_>>();
    assert_eq!(ids, ["deepseek-flash", "deepseek-v4-pro"]);
    assert_eq!(dsh["models"][0]["inputModalities"], json!(["text", "image"]));
    assert_eq!(item["runtimeVariants"][0]["models"], Value::Null, "codex lists through the profile models resource");

    // Delete: primary never; in-use refused (archived counts, retired not).
    assert_eq!(delete_variant(&ds.id, "codex").unwrap_err().code, "primary_variant_not_removable");
    bound_session("cutex.bound-dsh", &account(&ds.id), "dsh");
    let refused = delete_variant(&ds.id, "dsh").unwrap_err();
    assert_eq!((refused.code, refused.details["count"].clone()), ("variant_in_use", json!(1)));
    let item = profiles_api::list_resource().unwrap()["profiles"].as_array().unwrap().iter().find(|row| row["id"] == ds.id.as_str()).unwrap().clone();
    assert_eq!(item["runtimeVariants"][1]["inUse"], 1);
    crate::session::store::with_locked_session_rows(&crate::session::store::cutex_sessions_path().unwrap(), |rows| {
        rows.session_mut("cutex.bound-dsh").unwrap().archive_state = crate::session::model::CutexSessionArchiveState::Retired;
        rows.commit().map(drop)
    })
    .unwrap();
    settings_api::update_profile(&ds.id, &profiles_api::parse_patch(br#"{"defaultVariant":"dsh"}"#).unwrap()).unwrap();
    let after = delete_variant(&ds.id, "dsh").unwrap();
    assert!(after.variants.runtime_variants.is_empty());
    assert_eq!(default_variant_id(&after), "codex", "deleting the default resets it");
    assert_eq!(delete_variant(&ds.id, "dsh").unwrap_err().code, "unknown_variant");
}

#[test]
fn launch_plan_resolves_variant_binding_and_refuses_explicitly() {
    let _home = TestHome::new();
    let ds = api_profile("DS", "deepseek", Some(FAKE_DS_CODEX), None);
    add_variant(&ds.id, "dsh", None, Default::default()).unwrap();
    let ds = account(&ds.id);
    let primary = plan(&ds, None).unwrap();
    assert_eq!((primary.variant.id.as_str(), primary.runtime()), ("codex", &CliKind::Codex));
    let unknown = plan(&ds, Some("nope")).unwrap_err();
    assert_eq!(unknown.code, "unknown_variant");
    assert_eq!(unknown.details["variants"], json!(["codex", "dsh"]));
    // DSH is not installed in unit tests: a secondary must be available now.
    if !crate::components::dsh_runtime::advertised() {
        assert_eq!(plan(&ds, Some("dsh")).unwrap_err().code, "variant_runtime_unavailable");
    }
    let dsh_plan = LaunchPlan { account: ds.clone(), variant: variants(&ds).into_iter().find(|v| v.id == "dsh").unwrap() };
    let options = dsh_plan.dsh_options();
    assert_eq!(options.api_key_env, crate::profiles::stored_keys::DSH_STORED_KEY_ENV);
    assert_eq!(options.model, crate::profiles::dsh::DEFAULT_MODEL);
    options.validate().unwrap();
    let binding = dsh_plan.binding(Some(options.model.clone()), None, Some("0.2.1-rc.3".into()));
    assert_eq!((binding.variant_id.as_str(), binding.runtime.as_str(), binding.credential.source.as_str()), ("dsh", "dsh", "stored"));
    assert_eq!(crate::profiles::stored_keys::read_for_profile(&ds.id).unwrap().expose(), FAKE_DS_CODEX, "the variant shares the profile key");
    let wire = binding.to_wire();
    assert_eq!(wire["credential"]["envName"], Value::Null);
    assert!(!wire.to_string().contains(FAKE_DS_CODEX));

    // DSH stored source without a key: credential_missing before any side effect.
    let created = profiles_api::create_from_body(br#"{"name":"DSH empty","kind":"dsh","credential":{"source":"stored"}}"#).unwrap();
    assert_eq!(created["profile"]["credential"]["present"], false);
    let empty = account(created["profile"]["id"].as_str().unwrap());
    assert_eq!(plan(&empty, None).unwrap_err().code, "credential_missing");
    // Legacy env DSH profile: no credential check at plan time (as before).
    let legacy = profiles_api::create_from_body(br#"{"name":"DSH legacy","kind":"dsh"}"#).unwrap();
    let legacy = account(legacy["profile"]["id"].as_str().unwrap());
    let plan = plan(&legacy, None).unwrap();
    assert_eq!(plan.dsh_options().api_key_env, "DEEPSEEK_API_KEY");
    assert_eq!(plan.binding(None, None, None).credential.env_name.as_deref(), Some("DEEPSEEK_API_KEY"));
}

#[test]
fn dsh_credential_writes_are_write_only_versioned_and_explicit() {
    let _home = TestHome::new();
    let body = format!(r#"{{"name":"DSH key","kind":"dsh","apiKey":"{FAKE_DSH}","options":{{"dsh":{{"reasoningEffort":"max"}}}}}}"#);
    let created = profiles_api::create_from_body(body.as_bytes()).unwrap();
    assert_eq!(created["credentialsPresent"], true);
    let id = created["profile"]["id"].as_str().unwrap().to_string();
    assert_eq!(created["profile"]["credential"], json!({"source":"stored","present":true,"envName":null,"claudeAccountId":null,"kimiAccountId":null,"version":1,"writable":true}));
    assert_eq!(created["profile"]["authKind"], "apiKey");
    assert_eq!(created["profile"]["modelReasoningEffort"], "max");
    // Rotate: version 2.
    let patch = profiles_api::parse_patch(br#"{"apiKey":"sk-fake-r265-rotated"}"#).unwrap();
    settings_api::update_profile(&id, &patch).unwrap();
    assert_eq!(account(&id).variants.credential_version, 2);
    // Env source: explicit name only; reserved names refused.
    for bad in [br#"{"credential":{"source":"env"}}"#.as_slice(), br#"{"credential":{"source":"env","envName":"DSH_PROFILE_API_KEY"}}"#,
        br#"{"credential":{"source":"env","envName":"PATH"}}"#, br#"{"credential":{"source":"other"}}"#, br#"{"credential":{"source":"stored","envName":"X"}}"#] {
        assert!(profiles_api::parse_patch(bad).is_err(), "{}", String::from_utf8_lossy(bad));
    }
    let patch = profiles_api::parse_patch(br#"{"credential":{"source":"env","envName":"MY_DS_KEY"}}"#).unwrap();
    let (switched, _) = settings_api::update_profile(&id, &patch).unwrap();
    assert_eq!(credential_source(&switched), Source::Env);
    assert_eq!(switched.options.dsh.api_key_env, "MY_DS_KEY");
    assert!(crate::profiles::stored_keys::present(&id), "switching source keeps the stored file, unused");
    assert!(settings_api::update_profile(&id, &profiles_api::parse_patch(br#"{"credential":{"source":"env","envName":"MY_DS_KEY"},"apiKey":"k"}"#).unwrap()).is_err());
    // Clear removes the file.
    settings_api::update_profile(&id, &profiles_api::parse_patch(br#"{"credential":{"source":"stored"},"apiKey":null}"#).unwrap()).unwrap();
    assert!(!crate::profiles::stored_keys::present(&id));
    // Deleting the profile removes its key file.
    let doomed = profiles_api::create_from_body(br#"{"name":"DSH doomed","kind":"dsh","apiKey":"sk-fake-r265-doomed"}"#).unwrap();
    let doomed = doomed["profile"]["id"].as_str().unwrap().to_string();
    assert!(crate::profiles::stored_keys::present(&doomed));
    crate::profiles::lifecycle::delete_profile(&doomed).unwrap();
    assert!(!crate::profiles::stored_keys::key_path(&doomed).unwrap().exists());
    // Not on Codex / Claude profiles.
    let glm = api_profile("GLM", "zai", None, None);
    assert!(settings_api::update_profile(&glm.id, &profiles_api::parse_patch(br#"{"apiKey":null}"#).unwrap()).is_err());
    assert!(settings_api::update_profile(&glm.id, &profiles_api::parse_patch(br#"{"credential":{"source":"stored"}}"#).unwrap()).is_err());
    assert!(profiles_api::create_from_body(br#"{"name":"x","kind":"apiKey","credential":{"source":"stored"}}"#).is_err());
    assert!(profiles_api::create_from_body(br#"{"name":"y","kind":"dsh","apiKey":"k","credential":{"source":"env","envName":"MY_DS_KEY"}}"#).is_err());
    // Codex API-key writes bump the version only when the key changes.
    settings_api::update_profile(&glm.id, &profiles_api::parse_patch(br#"{"apiKey":"sk-test-a"}"#).unwrap()).unwrap();
    settings_api::update_profile(&glm.id, &profiles_api::parse_patch(br#"{"apiKey":"sk-test-a"}"#).unwrap()).unwrap();
    assert_eq!(account(&glm.id).variants.credential_version, 1);
}

/// Fake keys never appear in any client-visible output, store, export,
/// binding or error (accounts.json, profile items, events are built from items).
#[test]
fn fake_keys_never_leak_into_outputs_stores_exports_or_errors() {
    let _home = TestHome::new();
    let body = format!(r#"{{"name":"DSH secret","kind":"dsh","apiKey":"{FAKE_DSH}"}}"#);
    let dsh = profiles_api::create_from_body(body.as_bytes()).unwrap();
    let dsh_id = dsh["profile"]["id"].as_str().unwrap().to_string();
    let ds = api_profile("DS", "deepseek", Some(FAKE_DS_CODEX), None);
    add_variant(&ds.id, "dsh", None, Default::default()).unwrap();
    let claude = crate::runtime::claude_accounts::add(crate::runtime::claude_accounts::NewClaudeAccount {
        label: "api".into(),
        kind: crate::runtime::claude_accounts::ClaudeAccountKind::Api,
        api: Some(Default::default()),
        api_key: Some(FAKE_CLAUDE.into()),
    })
    .unwrap();
    bound_session("cutex.leak-check", &account(&dsh_id), "dsh");
    let mut outputs = vec![
        dsh.to_string(),
        profiles_api::list_resource().unwrap().to_string(),
        profiles_api::detail_item(&account(&dsh_id)).to_string(),
        profiles_api::detail_item(&account(&ds.id)).to_string(),
        String::from_utf8_lossy(&std::fs::read(crate::config::paths::accounts_path().unwrap()).unwrap()).into_owned(),
        serde_json::to_string(&crate::session::store::load_cutex_session_store().unwrap().sessions["cutex.leak-check"]).unwrap(),
        crate::profiles::transfer::export_profile(&dsh_id, false).unwrap().bundle.to_string(),
        crate::profiles::transfer::export_profile(&dsh_id, true).unwrap().bundle.to_string(),
        crate::profiles::transfer::export_profile(&ds.id, false).unwrap().bundle.to_string(),
        crate::runtime::claude_accounts::list_json().unwrap().to_string(),
        crate::profiles::catalog_api::models_resource(&account(&dsh_id)).unwrap().to_string(),
        format!("{:?}", account(&dsh_id)),
        format!("{:?}", profiles_api::parse_patch(format!(r#"{{"apiKey":"{FAKE_DSH}"}}"#).as_bytes()).unwrap()),
    ];
    // Errors mentioning credentials.
    let bad = format!(r#"{{"credential":{{"source":"env","envName":"{FAKE_DSH}"}}}}"#);
    outputs.push(profiles_api::parse_patch(bad.as_bytes()).unwrap_err());
    let bad = format!(r#"{{"name":"x","kind":"chatgpt","apiKey":"{FAKE_DSH}"}}"#);
    outputs.push(format!("{:?}", profiles_api::create_from_body(bad.as_bytes()).unwrap_err()));
    crate::profiles::stored_keys::remove(&dsh_id, 9).unwrap();
    outputs.push(format!("{:#}", crate::profiles::stored_keys::read_for_profile(&dsh_id).unwrap_err()));
    outputs.push(format!("{}", plan(&account(&dsh_id), None).unwrap_err()));
    for output in &outputs {
        for key in [FAKE_DSH, FAKE_DS_CODEX, FAKE_CLAUDE] {
            assert!(!output.contains(key), "key leaked into output: {}", &output[..output.len().min(400)]);
        }
    }
    // The owner's Claude API key write (desktop path) stored it write-only.
    assert!(crate::runtime::claude_accounts::api_key_present(&claude.id));
    let item = crate::runtime::claude_accounts::item_json(&crate::runtime::claude_accounts::load().unwrap(), &claude.id, None).unwrap();
    assert_eq!(item["api"]["apiKeyPresent"], true);
}

#[test]
fn claude_launch_key_is_bound_per_launch() {
    let _home = TestHome::new();
    let state = tempfile_dir();
    let source = state.join("account-key.json");
    std::fs::write(&source, br#"{"apiKey":"sk-fake-r265-claude-v1"}"#).unwrap();
    let mut config = json!({ "keyFile": source });
    crate::runtime::claude_accounts::bind_launch_key(&mut config, &state).unwrap();
    let bound = std::path::PathBuf::from(config["keyFile"].as_str().unwrap());
    assert_ne!(bound, source);
    std::fs::write(&source, br#"{"apiKey":"sk-fake-r265-claude-v2"}"#).unwrap();
    let raw: Value = serde_json::from_slice(&std::fs::read(&bound).unwrap()).unwrap();
    assert_eq!(raw["apiKey"], "sk-fake-r265-claude-v1", "a running launch keeps its key after rotation");
    let mut again = json!({ "keyFile": source });
    crate::runtime::claude_accounts::bind_launch_key(&mut again, &state).unwrap();
    let raw: Value = serde_json::from_slice(&std::fs::read(&bound).unwrap()).unwrap();
    assert_eq!(raw["apiKey"], "sk-fake-r265-claude-v2", "the next launch uses the new key");
    let _ = std::fs::remove_dir_all(&state);
}

fn tempfile_dir() -> std::path::PathBuf {
    let dir = crate::config::paths::config_dir().unwrap().join(format!("t-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn old_requests_behave_as_before() {
    // Old request bodies parse without variantId; unknown fields stay refused.
    let agent: crate::management::v2::human_agents::HumanCreateAgentRequest =
        serde_json::from_value(json!({"requestId":"r","name":"a","profile":"DS"})).unwrap();
    assert!(agent.variant_id.is_none());
    let agent: crate::management::v2::human_agents::HumanCreateAgentRequest =
        serde_json::from_value(json!({"requestId":"r","name":"a","profile":"DS","variantId":"dsh"})).unwrap();
    assert_eq!(agent.variant_id.as_deref(), Some("dsh"));
    assert!(serde_json::from_value::<crate::management::v2::human_agents::HumanCreateAgentRequest>(json!({"requestId":"r","name":"a","variantId":"Bad Id"})).unwrap().validate().is_err());
    let session: crate::management::v2::human_sessions::HumanCreateSessionRequest =
        serde_json::from_value(json!({"requestId":"r","profile":"DS"})).unwrap();
    assert!(session.variant_id.is_none());
    // A legacy record (no binding) round-trips without one.
    let record = crate::session::model::CutexSessionRecord::new("cutex.old".into(), None, "h".into(), "/tmp".into(), None).unwrap();
    let wire = serde_json::to_value(&record).unwrap();
    assert!(wire.get("profile_binding").is_none());
    // A legacy DSH profile body still creates an env-sourced profile with the same wire.
    let _home = TestHome::new();
    let created = profiles_api::create_from_body(br#"{"name":"DSH test","kind":"dsh","options":{"dsh":{"apiKeyEnv":"DSH_TEST_KEY"}}}"#).unwrap();
    assert_eq!(created["credentialsPresent"], false);
    assert_eq!(created["profile"]["options"]["dsh"]["apiKeyEnv"], "DSH_TEST_KEY");
    let stored = account(created["profile"]["id"].as_str().unwrap());
    assert_eq!(stored.variants, VariantState::default(), "nothing r265 is persisted for a legacy-shaped create");
}

#[test]
fn real_profile_items_and_bindings_match_the_exported_schema() {
    use crate::management::v2::schema_export::assert_valid;
    let _home = TestHome::new();
    let ds = api_profile("DS", "deepseek", Some(FAKE_DS_CODEX), None);
    add_variant(&ds.id, "dsh", None, Default::default()).unwrap();
    profiles_api::create_from_body(format!(r#"{{"name":"DSH","kind":"dsh","apiKey":"{FAKE_DSH}"}}"#).as_bytes()).unwrap();
    profiles_api::create_from_body(br#"{"name":"DSH env","kind":"dsh","credential":{"source":"env","envName":"MY_DS_KEY"}}"#).unwrap();
    create_profile(&NewProfile { name: "Claude".into(), kind: NewProfileKind::Claude, preset: None, base_url: None, model: None, api_key: None }).unwrap();
    create_profile(&NewProfile { name: "GPT".into(), kind: NewProfileKind::Chatgpt, preset: None, base_url: None, model: None, api_key: None }).unwrap();
    for item in profiles_api::list_resource().unwrap()["profiles"].as_array().unwrap() {
        assert_valid("profile-runtime-variants", "profileRuntimeFields", item);
    }
    let ds = account(&ds.id);
    assert_valid("profile-runtime-variants", "profileRuntimeFields", &profiles_api::detail_item(&ds));
    let plan = LaunchPlan { account: ds.clone(), variant: variants(&ds).into_iter().find(|v| v.id == "dsh").unwrap() };
    assert_valid("profile-runtime-variants", "launchBinding", &plan.binding(Some("deepseek-v4-flash".into()), None, Some("0.2.1-rc.3".into())).to_wire());
    assert_valid("profile-runtime-variants", "launchBinding", &plan.binding(None, None, None).to_wire());
    assert_valid("profile-runtime-variants", "variantCreate", &json!({"runtime":"dsh","defaults":{"model":"deepseek-v4-pro","reasoningEffort":"max"}}));
    assert_valid("profile-runtime-variants", "profileCredentialPatch", &json!({"apiKey":null,"credential":{"source":"env","envName":"MY_DS_KEY"},"defaultVariant":"dsh"}));
}

/// The `dsh` variant's plan (no dsh-runtime component in unit tests).
fn dsh_plan(account: &StoredAccount) -> LaunchPlan {
    LaunchPlan { account: account.clone(), variant: variants(account).into_iter().find(|v| v.id == "dsh").unwrap() }
}

fn set_recorded_version(id: &str, version: u64) {
    let mut store = settings_api::load_store().unwrap();
    store.accounts.iter_mut().find(|account| account.id == id).unwrap().variants.credential_version = version;
    crate::profiles::store::save_store(&store).unwrap();
}

/// C-6 (r265 E2E): the first stored key is version 1 (contract §1); a key
/// present with no recorded version (earlier data) reads as 1, and the next
/// write is 2. Nothing is rewritten by reading.
#[test]
fn stored_key_versions_start_at_one_and_unversioned_keys_read_as_one() {
    let _home = TestHome::new();
    let body = format!(r#"{{"name":"DS create","kind":"apiKey","preset":"deepseek","apiKey":"{FAKE_DS_CODEX}"}}"#);
    let created = profiles_api::create_from_body(body.as_bytes()).unwrap();
    assert_eq!(created["profile"]["credential"]["version"], 1, "{}", created["profile"]["credential"]);
    let id = created["profile"]["id"].as_str().unwrap().to_string();
    assert_eq!(account(&id).variants.credential_version, 1);
    // Without a key: version 0 until the first write, which is 1.
    let keyless = api_profile("DS keyless", "deepseek", None, None);
    assert_eq!(credential_view(&keyless)["version"], 0);
    settings_api::update_profile(&keyless.id, &profiles_api::parse_patch(br#"{"apiKey":"sk-test-first"}"#).unwrap()).unwrap();
    assert_eq!(account(&keyless.id).variants.credential_version, 1);

    // Data written before the fix: a Codex key with version 0.
    set_recorded_version(&id, 0);
    let before = std::fs::read(crate::config::paths::accounts_path().unwrap()).unwrap();
    let legacy = account(&id);
    assert_eq!(credential_view(&legacy)["version"], 1);
    profiles_api::list_resource().unwrap();
    assert_eq!(before, std::fs::read(crate::config::paths::accounts_path().unwrap()).unwrap(), "reading never rewrites the version");
    add_variant(&id, "dsh", None, Default::default()).unwrap();
    set_recorded_version(&id, 0);
    let legacy = account(&id);
    let plan = dsh_plan(&legacy);
    assert_eq!(plan.binding(None, None, None).credential.version, Some(1));
    settings_api::update_profile(&id, &profiles_api::parse_patch(br#"{"apiKey":"sk-test-rotated"}"#).unwrap()).unwrap();
    assert_eq!(account(&id).variants.credential_version, 2, "the next write after an unversioned key is 2");

    // A DSH stored key the same way.
    let dsh = profiles_api::create_from_body(format!(r#"{{"name":"DSH key","kind":"dsh","apiKey":"{FAKE_DSH}"}}"#).as_bytes()).unwrap();
    let dsh_id = dsh["profile"]["id"].as_str().unwrap().to_string();
    assert_eq!(dsh["profile"]["credential"]["version"], 1);
    set_recorded_version(&dsh_id, 0);
    assert_eq!(credential_view(&account(&dsh_id))["version"], 1);
    settings_api::update_profile(&dsh_id, &profiles_api::parse_patch(br#"{"apiKey":"sk-fake-r265-dsh-next"}"#).unwrap()).unwrap();
    assert_eq!(account(&dsh_id).variants.credential_version, 2);
    // Removal counts as a write; an absent key never reads as version 1.
    settings_api::update_profile(&dsh_id, &profiles_api::parse_patch(br#"{"apiKey":null}"#).unwrap()).unwrap();
    assert_eq!(credential_view(&account(&dsh_id))["version"], 3);
    let env = profiles_api::create_from_body(br#"{"name":"DSH env","kind":"dsh"}"#).unwrap();
    assert_eq!(env["profile"]["credential"]["version"], Value::Null);
}

/// C-1 (r265 E2E): a fork's binding names the source's profile, variant,
/// runtime and credential source; the stored key's version is the one the
/// fork's start reads (current), and boundAt is new.
#[test]
fn fork_binding_keeps_profile_variant_and_credential_with_the_current_version() {
    let _home = TestHome::new();
    let ds = api_profile("DS", "deepseek", Some(FAKE_DS_CODEX), None);
    add_variant(&ds.id, "dsh", None, Default::default()).unwrap();
    let source = dsh_plan(&account(&ds.id)).binding(Some("deepseek-flash".into()), Some("low".into()), Some("0.2.1-rc.3".into()));
    assert_eq!(source.credential.version, Some(1));
    settings_api::update_profile(&ds.id, &profiles_api::parse_patch(br#"{"apiKey":"sk-test-rotated"}"#).unwrap()).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let fork = source.for_fork(Some("deepseek-v4-pro".into()), Some("high".into()), Some("0.2.1-rc.3".into()));
    assert_eq!((fork.profile_id.as_str(), fork.variant_id.as_str(), fork.runtime.as_str()), (ds.id.as_str(), "dsh", "dsh"));
    assert_eq!(fork.credential, BoundCredential { source: "stored".into(), env_name: None, version: Some(2), kimi_account_id: None });
    assert_eq!((fork.model.as_deref(), fork.reasoning_effort.as_deref()), (Some("deepseek-v4-pro"), Some("high")));
    assert_ne!(fork.bound_at, source.bound_at);
    // An env binding keeps its variable name; no version.
    let env = profiles_api::create_from_body(br#"{"name":"DSH env","kind":"dsh"}"#).unwrap();
    let env_id = env["profile"]["id"].as_str().unwrap().to_string();
    let env_binding = plan(&account(&env_id), None).unwrap().binding(None, None, None);
    assert_eq!(env_binding.for_fork(None, None, None).credential, env_binding.credential);
    // The source's profile is gone: keep the bound version (the start fails credential_missing).
    crate::profiles::lifecycle::delete_profile(&ds.id).unwrap();
    assert_eq!(source.for_fork(None, None, None).credential.version, Some(1));
}

/// QA (host-a): the key file and the profile store are two writes. The key
/// file now carries the key and its version in one atomic write and is the
/// authority for a stored key's version; a failed store save restores the
/// previous key file; a crash in between can no longer pair the new key
/// with the old version.
#[test]
fn key_rotation_cannot_leave_key_and_version_apart() {
    let _home = TestHome::new();
    let created = profiles_api::create_from_body(format!(r#"{{"name":"DSH rotate","kind":"dsh","apiKey":"{FAKE_DSH}"}}"#).as_bytes()).unwrap();
    let id = created["profile"]["id"].as_str().unwrap().to_string();
    assert_eq!(credential_view(&account(&id))["version"], 1);
    // Crash after the key file write, before the store save.
    let next = crate::profiles::stored_keys::SecretKey::new("sk-fake-r265-crash-next").unwrap();
    crate::profiles::stored_keys::write(&id, &next, 2).unwrap();
    assert_eq!(account(&id).variants.credential_version, 1, "the store never saw the rotation");
    assert_eq!(credential_view(&account(&id))["version"], 2, "the version is the one written with the key");
    assert_eq!(dsh_plan(&account(&id)).binding(None, None, None).credential.version, Some(2));
    // The next rotation continues from there.
    settings_api::update_profile(&id, &profiles_api::parse_patch(br#"{"apiKey":"sk-fake-r265-after-crash"}"#).unwrap()).unwrap();
    assert_eq!(credential_view(&account(&id))["version"], 3);
    // A failed store save: the PATCH fails and the stored key is unchanged.
    crate::profiles::settings_api::FAIL_PATCH_SAVE.with(|fail| fail.set(true));
    let failed = settings_api::update_profile(&id, &profiles_api::parse_patch(br#"{"apiKey":"sk-fake-r265-never-saved"}"#).unwrap());
    assert!(failed.is_err());
    assert_eq!(crate::profiles::stored_keys::read_for_profile(&id).unwrap().expose(), "sk-fake-r265-after-crash");
    assert_eq!(credential_view(&account(&id))["version"], 3);
    assert!(!crate::profiles::stored_keys::retired_values(&id).contains(&"sk-fake-r265-never-saved".to_string()));
}

/// Rotation keeps earlier keys for redaction: DSH stored keys (key file) and
/// Codex API-key profiles (auth.json; retired values in the key file).
#[test]
fn rotated_keys_stay_known_for_redaction() {
    let _home = TestHome::new();
    let created = profiles_api::create_from_body(br#"{"name":"DSH retired","kind":"dsh","apiKey":"sk-fake-r265-ret-a"}"#).unwrap();
    let id = created["profile"]["id"].as_str().unwrap().to_string();
    settings_api::update_profile(&id, &profiles_api::parse_patch(br#"{"apiKey":"sk-fake-r265-ret-b"}"#).unwrap()).unwrap();
    settings_api::update_profile(&id, &profiles_api::parse_patch(br#"{"apiKey":null}"#).unwrap()).unwrap();
    assert_eq!(crate::profiles::stored_keys::retired_values(&id), ["sk-fake-r265-ret-a", "sk-fake-r265-ret-b"]);
    assert_eq!(credential_view(&account(&id))["present"], false);
    let glm = api_profile("GLM retired", "zai", Some("sk-fake-r265-codex-a"), None);
    settings_api::update_profile(&glm.id, &profiles_api::parse_patch(br#"{"apiKey":"sk-fake-r265-codex-b"}"#).unwrap()).unwrap();
    assert_eq!(crate::profiles::stored_keys::retired_values(&glm.id), ["sk-fake-r265-codex-a"]);
    // Nothing of it in any client-visible profile output.
    let listed = profiles_api::list_resource().unwrap().to_string();
    assert!(!listed.contains("sk-fake-r265-ret-") && !listed.contains("sk-fake-r265-codex-a"));
    crate::profiles::lifecycle::delete_profile(&id).unwrap();
    assert!(crate::profiles::stored_keys::retired_values(&id).is_empty(), "the profile's key file goes with it");
}
