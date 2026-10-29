use super::*;

use crate::profiles::kimi_accounts::tests::{logged_in_account, token_json, TempDir, FAKE_REFRESH_A, FAKE_TOKEN_A};
use crate::profiles::kimi_accounts::{self, RefreshLock};
use crate::profiles::lifecycle::tests::TestHome;
use crate::profiles::runtime_variants::{self as variants, Status, VariantDefaults};
use crate::management::v2::profiles_api;

const FAKE_KEY: &str = "sk-fake-r266-kimi-key-a";

pub(crate) fn kimi_supported() -> (Status, Option<&'static str>) {
    (Status::Supported, None)
}

fn store_account(id: &str) -> crate::profiles::model::StoredAccount {
    crate::profiles::settings_api::load_store().unwrap().accounts.into_iter().find(|account| account.id == id).unwrap()
}

/// Simulates Kimi's `FileTokenStorage.save`: tmp file + rename in the
/// directory the process sees (here: the agent home's linked `credentials`).
fn kimi_save(agent_home: &Path, slot: &str, body: &str) {
    let dir = agent_home.join("credentials");
    let tmp = dir.join(format!("{slot}.json.tmp.{}", std::process::id()));
    std::fs::write(&tmp, body).unwrap();
    std::fs::rename(&tmp, dir.join(format!("{slot}.json"))).unwrap();
}

#[test]
fn api_key_variant_resolves_the_profile_key_and_follows_rotation() {
    let _home = TestHome::new();
    crate::profiles::kimi::set_component_probe(kimi_supported);
    let body = format!(r#"{{"name":"Kimi Code key","preset":"kimi-code-cn","apiKey":"{FAKE_KEY}"}}"#);
    let created = profiles_api::create_from_body(body.as_bytes()).unwrap();
    let id = created["profile"]["id"].as_str().unwrap().to_string();
    let profile = &created["profile"];
    assert_eq!(profile["defaultVariant"], "codex");
    let kimi_row = profile["runtimeCompatibility"].as_array().unwrap().iter().find(|row| row["runtime"] == "kimi").unwrap().clone();
    assert_eq!(kimi_row["status"], "supported");
    assert!(profile["providerInterfaces"].as_array().unwrap().iter().any(|row| row["kind"] == "kimiNative"));

    let account = variants::add_variant(&id, "kimi", None, VariantDefaults { model: None, reasoning_effort: Some("high".into()) }).unwrap();
    assert_eq!(variants::default_variant_id(&account), "codex", "codex stays the default");
    let plan = variants::plan(&account, Some("kimi")).unwrap();
    assert_eq!(plan.kimi_options(), (Some("kimi-for-coding".into()), Some("high".into())));
    let binding = plan.binding(Some("kimi-for-coding".into()), Some("high".into()), Some("2.3.3".into()));
    assert_eq!((binding.runtime.as_str(), binding.credential.source.as_str()), ("kimi", "stored"));
    assert_eq!(binding.credential.version, Some(1));
    assert!(binding.to_wire()["credential"]["kimiAccountId"].is_null());

    let launch = resolve(&binding).unwrap();
    let (name, key) = launch.secret_env.as_ref().unwrap();
    assert_eq!((name.as_str(), key.expose()), (KIMI_STORED_KEY_ENV, FAKE_KEY));
    assert_eq!(launch.provider_id, API_KEY_PROVIDER_ID);
    let provider = &launch.config["providers"][API_KEY_PROVIDER_ID];
    assert_eq!(provider["type"].as_str(), Some("kimi"));
    assert_eq!(provider["base_url"].as_str(), Some("https://api.kimi.com/coding/v1"));
    assert_eq!(provider["api_key_env"].as_str(), Some(KIMI_STORED_KEY_ENV));
    assert_eq!(launch.redaction_values(), vec![FAKE_KEY.to_string()]);
    // Redaction: the key is in no printable form of the launch.
    let config_text = toml::to_string(&launch.config).unwrap();
    for text in [format!("{launch:?}"), config_text] {
        assert!(!text.contains(FAKE_KEY), "{text}");
    }

    // Rotation applies to the next start.
    let patch = profiles_api::parse_patch(br#"{"apiKey":"sk-fake-r266-kimi-key-b"}"#).unwrap();
    crate::profiles::settings_api::update_profile(&id, &patch).unwrap();
    assert_eq!(resolve(&binding).unwrap().secret_env.unwrap().1.expose(), "sk-fake-r266-kimi-key-b");
    assert_eq!(variants::credential_version(&store_account(&id)), 2);

    // A binding never falls back to a host variable.
    std::env::set_var(KIMI_STORED_KEY_ENV, "sk-fake-host-must-not-be-used");
    let mut gone = binding.clone();
    gone.profile_id = "no-such-profile".into();
    let error = resolve(&gone).unwrap_err();
    std::env::remove_var(KIMI_STORED_KEY_ENV);
    assert_eq!(error.code(), "credential_missing");
    assert!(!error.to_string().contains("sk-fake"));
    // Wrong runtime / unknown source.
    let mut wrong = binding.clone();
    wrong.runtime = "dsh".into();
    assert_eq!(resolve(&wrong).unwrap_err().code(), "invalid_binding");
    wrong = binding.clone();
    wrong.credential.source = "env".into();
    assert_eq!(resolve(&wrong).unwrap_err().code(), "invalid_binding");

    // API-key homes never keep account links.
    let agent = TempDir::new();
    let stray = TempDir::new();
    link_dir(stray.path(), &agent.path().join("credentials")).unwrap();
    launch_install_api_key(&binding, agent.path());
    assert!(!agent.path().join("credentials").exists());
}

fn launch_install_api_key(binding: &ProfileLaunchBinding, home: &Path) {
    resolve(binding).unwrap().install_into(home).unwrap();
}

#[test]
fn kimi_column_follows_provider_and_host() {
    let _home = TestHome::new();
    crate::profiles::kimi::set_component_probe(kimi_supported);
    let create = |body: &str| -> String {
        profiles_api::create_from_body(body.as_bytes()).unwrap()["profile"]["id"].as_str().unwrap().to_string()
    };
    let relay = create(r#"{"name":"Relay","preset":"custom","baseUrl":"https://relay.example/v1","apiKey":"sk-fake-relay"}"#);
    let moonshot = create(r#"{"name":"Moonshot","preset":"moonshot","apiKey":"sk-fake-moon"}"#);
    let deepseek = create(r#"{"name":"DeepSeek","preset":"deepseek","apiKey":"sk-fake-ds"}"#);
    let row = |id: &str, runtime: &str| -> (String, Option<String>) {
        let (status, reason) = variants::compatibility(&store_account(id), &runtime.parse().unwrap());
        (status.as_str().to_string(), reason.map(str::to_string))
    };
    assert_eq!(row(&relay, "kimi"), ("unsupported".into(), Some("providerNotKimiNative".into())));
    assert_eq!(row(&deepseek, "kimi"), ("unsupported".into(), Some("providerNotKimiNative".into())));
    assert_eq!(row(&moonshot, "kimi"), ("supported".into(), None));
    assert_eq!(row(&moonshot, "claude"), ("unsupported".into(), Some("anthropicRouteNotYetSupported".into())));
    let refused = variants::add_variant(&relay, "kimi", None, VariantDefaults::default()).unwrap_err();
    assert_eq!((refused.status, refused.code), (409, "variant_runtime_unsupported"));
    // Moonshot: the kimi variant defaults to the profile's own model.
    let account = variants::add_variant(&moonshot, "kimi", None, VariantDefaults::default()).unwrap();
    assert_eq!(variants::plan(&account, Some("kimi")).unwrap().kimi_options().0.as_deref(), Some("kimi-k3"));
    assert!(variants::add_variant(&moonshot, "kimi", Some("kimi-2"), VariantDefaults::default()).is_err(), "one kimi variant");
    assert!(variants::patch_variant(&moonshot, "kimi", crate::config::global_settings::ConfigValueUpdate::Unchanged,
        crate::config::global_settings::ConfigValueUpdate::Set("xhigh".into())).is_err());
    // Presets preview the same column.
    let preview = variants::preset_compatibility_view("kimi-code", Some("https://api.kimi.ai/coding/v1"));
    assert_eq!(preview[3]["runtime"], "kimi");
    assert_eq!(preview[3]["status"], "supported");
    assert_eq!(variants::preset_compatibility_view("zai", Some("https://api.z.ai/api/paas/v4"))[3]["reason"], "providerNotKimiNative");
    // Component missing: shown as componentNotInstalled, the variant cannot launch.
    crate::profiles::kimi::set_component_probe(|| (Status::ComponentNotInstalled, Some("componentNotInstalled")));
    let refusal = variants::plan(&store_account(&moonshot), Some("kimi")).unwrap_err();
    assert_eq!(refusal.code, "variant_runtime_unavailable");
    crate::profiles::kimi::set_component_probe(kimi_supported);
}

/// profile-relay-runtimes (2026-10-28): a Moonshot / Kimi Code key behind a
/// relay keeps Kimi Code, and the launch config carries the relay address
/// (B2, B3); the model family follows the provider id (B8).
#[test]
fn kimi_relay_profiles_keep_kimi_code_and_launch_on_the_relay() {
    let _home = TestHome::new();
    crate::profiles::kimi::set_component_probe(kimi_supported);
    let create = |body: &str| -> String {
        profiles_api::create_from_body(body.as_bytes()).unwrap()["profile"]["id"].as_str().unwrap().to_string()
    };
    let relay_profile = create(r#"{"name":"RelayProfile","preset":"moonshot","baseUrl":"https://relay.example/v1","model":"kimi-k3","apiKey":"sk-fake-relay_profile"}"#);
    let coding = create(r#"{"name":"Coding relay","preset":"kimi-code","baseUrl":"http://127.0.0.1:9/kimi/v1","apiKey":"sk-fake-coding"}"#);
    for id in [&relay_profile, &coding] {
        let account = store_account(id);
        assert_eq!(variants::compatibility(&account, &"kimi".parse().unwrap()), (Status::Supported, None));
        let item = profiles_api::detail_item(&account);
        let row = item["providerInterfaces"].as_array().unwrap().iter().find(|row| row["kind"] == "kimiNative").unwrap().clone();
        assert_eq!((row["official"].clone(), row["relayAccepted"].clone()), (json!(false), json!(true)), "{row}");
    }
    // Presets: the provider decides, never the default host.
    assert_eq!(variants::preset_compatibility_view("moonshot", None)[3]["status"], "supported");

    let account = variants::add_variant(&relay_profile, "kimi", None, VariantDefaults::default()).unwrap();
    let plan = variants::plan(&account, Some("kimi")).unwrap();
    assert_eq!(plan.kimi_options().0.as_deref(), Some("kimi-k3"));
    let launch = resolve(&plan.binding(None, None, None)).unwrap();
    assert_eq!(launch.config["providers"][API_KEY_PROVIDER_ID]["base_url"].as_str(), Some("https://relay.example/v1"));
    assert_eq!(launch.model_family, Some(crate::profiles::kimi::ModelFamily::Moonshot));

    let account = variants::add_variant(&coding, "kimi", None, VariantDefaults::default()).unwrap();
    let plan = variants::plan(&account, Some("kimi")).unwrap();
    assert_eq!(plan.kimi_options().0.as_deref(), Some(crate::profiles::kimi::KIMI_CODE_DEFAULT_MODEL));
    let launch = resolve(&plan.binding(None, None, None)).unwrap();
    assert_eq!(launch.config["providers"][API_KEY_PROVIDER_ID]["base_url"].as_str(), Some("http://127.0.0.1:9/kimi/v1"));
    assert_eq!(launch.model_family, Some(crate::profiles::kimi::ModelFamily::KimiCode));
    assert_eq!(launch.default_model.as_deref(), Some(crate::profiles::kimi::KIMI_CODE_DEFAULT_MODEL));
}

#[test]
fn kimi_account_profiles_bind_the_account_and_share_one_credential_dir() {
    let _home = TestHome::new();
    crate::profiles::kimi::set_component_probe(kimi_supported);
    let account = logged_in_account("Sub");
    let body = format!(r#"{{"name":"Kimi sub","kind":"kimi","kimiAccount":"{}"}}"#, account.id);
    let created = profiles_api::create_from_body(body.as_bytes()).unwrap();
    let profile = &created["profile"];
    let id = profile["id"].as_str().unwrap().to_string();
    assert_eq!(profile["cliKind"], "kimi");
    assert_eq!(profile["kimiAccount"], account.id.as_str());
    assert_eq!(profile["credential"]["source"], "kimiAccount");
    assert_eq!(profile["credential"]["kimiAccountId"], account.id.as_str());
    assert_eq!(profile["credential"]["present"], true);
    assert_eq!(profile["runtimeVariants"][0]["id"], "kimi");
    assert_eq!(profile["runtimeVariants"][0]["models"][0]["id"], "kimi-code/kimi-for-coding");
    let rows = profile["runtimeCompatibility"].as_array().unwrap();
    assert!(rows.iter().filter(|row| row["runtime"] != "kimi").all(|row| row["reason"] == "kimiAccountKimiOnly"), "{rows:?}");
    assert!(!profile.to_string().contains(FAKE_TOKEN_A));
    // Models resource: the account's catalog.
    let models = crate::profiles::catalog_api::models_resource(&store_account(&id)).unwrap();
    assert_eq!(models["modelsSource"]["kind"], "kimi");
    assert_eq!(models["selectedModel"], "kimi-code/kimi-for-coding");
    // Defaults: model in the catalog, Kimi efforts.
    let patch = profiles_api::parse_patch(br#"{"model":"kimi-code/kimi-k3-fast","modelReasoningEffort":"low"}"#).unwrap();
    crate::profiles::settings_api::update_profile(&id, &patch).unwrap();
    assert!(crate::profiles::settings_api::update_profile(&id, &profiles_api::parse_patch(br#"{"model":"gpt-5"}"#).unwrap()).is_err());
    assert!(crate::profiles::settings_api::update_profile(&id, &profiles_api::parse_patch(br#"{"apiKey":"sk-x"}"#).unwrap()).is_err());
    assert!(crate::profiles::settings_api::update_profile(&id, &profiles_api::parse_patch(br#"{"kimiAccount":"ka-missing"}"#).unwrap()).is_err());

    let stored = store_account(&id);
    let plan = variants::plan(&stored, None).unwrap();
    assert_eq!(plan.kimi_options(), (Some("kimi-code/kimi-k3-fast".into()), Some("low".into())));
    let binding = plan.binding(Some("kimi-code/kimi-k3-fast".into()), Some("low".into()), Some("2.3.3".into()));
    assert_eq!(binding.credential.source, "kimiAccount");
    assert_eq!(binding.credential.kimi_account_id.as_deref(), Some(account.id.as_str()));
    assert_eq!(binding.to_wire()["credential"]["kimiAccountId"], account.id.as_str());

    let launch = resolve(&binding).unwrap();
    assert!(launch.secret_env.is_none() && launch.redaction_values().is_empty());
    assert_eq!(launch.provider_id, "managed:kimi-code");
    let text = toml::to_string(&launch.config).unwrap();
    assert!(text.contains("oauth") && text.contains("kimi-code/kimi-for-coding") && !text.contains("\"mine\""), "{text}");
    assert!(!text.contains(FAKE_TOKEN_A) && !text.contains(FAKE_REFRESH_A));
    assert_eq!(launch.default_model.as_deref(), Some("kimi-code/kimi-for-coding"));

    // Two agent homes, one account home.
    let account_home = kimi_accounts::account_home(&account.id).unwrap();
    let agents = TempDir::new();
    let (a, b) = (agents.path().join("a").join("kimi"), agents.path().join("b").join("kimi"));
    launch.install_into(&a).unwrap();
    launch.install_into(&b).unwrap();
    launch.install_into(&a).unwrap(); // idempotent
    for home in [&a, &b] {
        for name in ["credentials", "oauth"] {
            assert!(std::fs::symlink_metadata(home.join(name)).unwrap().file_type().is_symlink() || cfg!(windows));
            assert_eq!(std::fs::canonicalize(home.join(name)).unwrap(), std::fs::canonicalize(account_home.join(name)).unwrap());
        }
        assert_eq!(std::fs::read_to_string(home.join("device_id")).unwrap(), "device-r266");
    }
    // A refresh written by agent A (tmp + rename through its link) is the
    // account's file: B and cutex see it, and A's link is still a link.
    kimi_save(&a, "kimi-code", &token_json("fake-access-rotated", "fake-refresh-rotated"));
    assert!(std::fs::symlink_metadata(a.join("credentials")).unwrap().file_type().is_symlink());
    let seen_by_b = std::fs::read_to_string(b.join("credentials/kimi-code.json")).unwrap();
    assert!(seen_by_b.contains("fake-refresh-rotated"));
    assert!(std::fs::read_to_string(account_home.join("credentials/kimi-code.json")).unwrap().contains("fake-refresh-rotated"));
    // Both agents' lock paths are one lock.
    let held = RefreshLock::acquire(&a, "kimi-code").unwrap();
    assert!(RefreshLock::acquire_with(&b, "kimi-code", std::time::Duration::from_millis(200)).is_err());
    assert!(RefreshLock::acquire_with(&account_home, "kimi-code", std::time::Duration::from_millis(200)).is_err());
    drop(held);
    // The rejected copy strategy: a copied file goes stale on the first rotation.
    let c = agents.path().join("c").join("kimi");
    std::fs::create_dir_all(c.join("credentials")).unwrap();
    std::fs::copy(account_home.join("credentials/kimi-code.json"), c.join("credentials/kimi-code.json")).unwrap();
    kimi_save(&a, "kimi-code", &token_json("fake-access-rotated-2", "fake-refresh-rotated-2"));
    assert!(!std::fs::read_to_string(c.join("credentials/kimi-code.json")).unwrap().contains("rotated-2"));
    assert!(std::fs::read_to_string(b.join("credentials/kimi-code.json")).unwrap().contains("rotated-2"));
    // A real dir where a link belongs is moved aside, never merged.
    launch.install_into(&c).unwrap();
    assert!(std::fs::read_to_string(c.join("credentials/kimi-code.json")).unwrap().contains("rotated-2"));
    assert!(std::fs::read_dir(&c).unwrap().any(|entry| entry.unwrap().file_name().to_string_lossy().starts_with("credentials.replaced-")));

    // Logout: one tombstone, seen through every link; launches refused.
    kimi_accounts::logout(&account.id).unwrap();
    for home in [&a, &b] {
        assert_eq!(kimi_accounts::read_credential_state(&home.join("credentials/kimi-code.json")), kimi_accounts::CredentialState::Revoked);
    }
    assert_eq!(resolve(&binding).unwrap_err().code(), "kimi_account_logged_out");
    assert_eq!(variants::plan(&store_account(&id), None).unwrap_err().code, "kimi_account_logged_out");
    // Account delete is refused while the profile references it.
    assert!(matches!(kimi_accounts::remove(&account.id, false), Err(kimi_accounts::RemoveError::InUse(_))));
    // An unbound Kimi profile cannot launch.
    let patch = profiles_api::parse_patch(br#"{"kimiAccount":null}"#).unwrap();
    crate::profiles::settings_api::update_profile(&id, &patch).unwrap();
    assert_eq!(variants::plan(&store_account(&id), None).unwrap_err().code, "credential_missing");
    let mut missing = binding.clone();
    missing.credential.kimi_account_id = Some("ka-gone".into());
    assert_eq!(resolve(&missing).unwrap_err().code(), "kimi_account_not_found");
}

#[test]
fn kimi_profile_create_is_validated() {
    let _home = TestHome::new();
    for body in [
        r#"{"name":"k","kind":"kimi","kimiAccount":"ca-abc"}"#,
        r#"{"name":"k","kind":"kimi","apiKey":"sk-x"}"#,
        r#"{"name":"k","kind":"kimi","preset":"kimi-code"}"#,
        r#"{"name":"k","kind":"apiKey","kimiAccount":"ka-abc"}"#,
        r#"{"name":"k","kind":"kimi","model":"a b"}"#,
    ] {
        assert!(profiles_api::parse_create(body.as_bytes()).is_err() || profiles_api::create_from_body(body.as_bytes()).is_err(), "{body}");
    }
    // An unknown account is refused and nothing is left behind.
    assert!(profiles_api::create_from_body(br#"{"name":"k","kind":"kimi","kimiAccount":"ka-nothere"}"#).is_err());
    assert!(crate::profiles::settings_api::load_store().unwrap().accounts.iter().all(|account| account.name != "k"));
    // Without an account: created, credential none, launches refused.
    let created = profiles_api::create_from_body(br#"{"name":"k2","kind":"kimi"}"#).unwrap();
    assert_eq!(created["profile"]["credential"]["source"], "none");
    assert_eq!(created["kind"], "kimi");
    // The store round-trips and the new fields stay top-level additive.
    let raw = std::fs::read_to_string(crate::config::paths::accounts_path().unwrap()).unwrap();
    assert!(raw.contains("\"cli_kind\": \"kimi\""), "{raw}");
}

#[test]
fn kimi_items_logins_and_bindings_match_the_exported_schemas() {
    use crate::management::v2::schema_export::assert_valid;
    let _home = TestHome::new();
    crate::profiles::kimi::set_component_probe(kimi_supported);
    let fresh = kimi_accounts::add("Fresh", kimi_accounts::KimiRegion::Global).unwrap();
    let account = logged_in_account("Schema");
    let list = kimi_accounts::list_json().unwrap();
    assert_valid("kimi-accounts", "accountList", &list);
    let registry = kimi_accounts::load().unwrap();
    for id in [&fresh.id, &account.id] {
        assert_valid("kimi-accounts", "account", &kimi_accounts::item_json(&registry, id, None).unwrap());
    }
    let login = kimi_accounts::LoginSnapshot {
        login_id: "kl-0123456789abcdef0123456789abcdef".into(), account_id: fresh.id.clone(),
        state: kimi_accounts::LoginState::Pending, verification_uri: Some("https://www.kimi.com/x?user_code=A-1".into()),
        user_code: Some("A-1".into()), expires_at: Some("2026-10-27T00:10:00Z".into()), interval: 5,
        started_at: "2026-10-27T00:00:00Z".into(), finished_at: None, error: None,
    };
    assert_valid("kimi-accounts", "login", &login.to_wire());
    let failed = kimi_accounts::LoginSnapshot { state: kimi_accounts::LoginState::Denied, error: Some(("kimi_login_denied", "no".into())), ..login };
    assert_valid("kimi-accounts", "login", &failed.to_wire());

    let body = format!(r#"{{"name":"Kimi sub","kind":"kimi","kimiAccount":"{}"}}"#, account.id);
    let id = profiles_api::create_from_body(body.as_bytes()).unwrap()["profile"]["id"].as_str().unwrap().to_string();
    let kimi_profile = store_account(&id);
    assert_valid("profile-runtime-variants", "profileRuntimeFields", &profiles_api::detail_item(&kimi_profile));
    let binding = variants::plan(&kimi_profile, None).unwrap().binding(None, None, Some("2.3.3".into()));
    assert_valid("profile-runtime-variants", "launchBinding", &binding.to_wire());
    let key_id = profiles_api::create_from_body(br#"{"name":"Kimi key","preset":"kimi-code","apiKey":"sk-fake-r266-schema"}"#).unwrap()["profile"]["id"]
        .as_str().unwrap().to_string();
    let with_variant = variants::add_variant(&key_id, "kimi", None, VariantDefaults::default()).unwrap();
    let item = profiles_api::detail_item(&with_variant);
    assert_valid("profile-runtime-variants", "profileRuntimeFields", &item);
    assert!(!item.to_string().contains("sk-fake-r266-schema"));
    let binding = variants::plan(&with_variant, Some("kimi")).unwrap().binding(None, None, None);
    assert_valid("profile-runtime-variants", "launchBinding", &binding.to_wire());
}

#[test]
fn kimi_profile_export_never_carries_the_account_or_its_login() {
    let _home = TestHome::new();
    let account = logged_in_account("Export");
    let body = format!(r#"{{"name":"Kimi export","kind":"kimi","kimiAccount":"{}","model":"kimi-code/kimi-for-coding"}}"#, account.id);
    profiles_api::create_from_body(body.as_bytes()).unwrap();
    let export = crate::profiles::transfer::export_profile("Kimi export", true).unwrap();
    let text = export.bundle.to_string();
    assert!(!text.contains(&account.id) && !text.contains(FAKE_TOKEN_A) && !text.contains(FAKE_REFRESH_A), "{text}");
    assert_eq!(export.bundle["profile"]["cli_kind"], "kimi");
    assert!(!export.credentials_included);
    let imported = crate::profiles::transfer::import_profile(&export.bundle, "Kimi imported").unwrap();
    assert_eq!(imported.account.cli_kind, crate::profiles::model::CliKind::Kimi);
    assert_eq!(imported.account.variants.kimi_account, None);
    assert_eq!(imported.account.variants.kimi_defaults.as_ref().and_then(|d| d.model.as_deref()), Some("kimi-code/kimi-for-coding"));
    assert!(imported.warnings.iter().any(|warning| warning["code"] == "kimiAccountRequired"));
}

#[test]
fn nothing_kimi_is_planned_without_the_component() {
    let _home = TestHome::new();
    let account = logged_in_account("NoComponent");
    let body = format!(r#"{{"name":"Kimi nc","kind":"kimi","kimiAccount":"{}"}}"#, account.id);
    let id = profiles_api::create_from_body(body.as_bytes()).unwrap()["profile"]["id"].as_str().unwrap().to_string();
    crate::profiles::kimi::set_component_probe(|| (Status::ComponentNotInstalled, Some("componentNotInstalled")));
    let refusal = variants::plan(&store_account(&id), None).unwrap_err();
    crate::profiles::kimi::set_component_probe(kimi_supported);
    assert_eq!(refusal.code, "variant_runtime_unavailable");
    assert!(variants::plan(&store_account(&id), None).is_ok());
}

#[test]
fn kimi_profile_items_show_their_defaults_like_other_primaries() {
    let _home = TestHome::new();
    let account = logged_in_account("Items");
    let body = format!(r#"{{"name":"Kimi items","kind":"kimi","kimiAccount":"{}"}}"#, account.id);
    let created = profiles_api::create_from_body(body.as_bytes()).unwrap();
    assert_eq!(created["profile"]["model"], "kimi-code/kimi-for-coding");
    assert_eq!(created["profile"]["runtimeVariants"][0]["defaults"]["model"], "kimi-code/kimi-for-coding");
    let id = created["profile"]["id"].as_str().unwrap().to_string();
    let patch = profiles_api::parse_patch(br#"{"modelReasoningEffort":"max"}"#).unwrap();
    crate::profiles::settings_api::update_profile(&id, &patch).unwrap();
    let item = profiles_api::detail_item(&store_account(&id));
    assert_eq!(item["modelReasoningEffort"], "max");
    assert_eq!(item["runtimeVariants"][0]["defaults"]["reasoningEffort"], "max");
    assert_eq!(item["detail"]["editable"]["model"], true);
    assert_eq!(item["authKind"], "kimi");
}
