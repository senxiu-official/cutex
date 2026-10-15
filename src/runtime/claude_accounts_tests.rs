use super::*;

use crate::profiles::lifecycle::tests::TestHome;
use crate::profiles::model::StoredAccount;
use crate::runtime::claude::{ClaudeDeployment, ClaudeSession};

fn profile(name: &str, cli_kind: &str, claude_account: Option<&str>) -> StoredAccount {
    let mut profile: StoredAccount =
        serde_json::from_value(json!({ "id": format!("id-{name}"), "name": name, "cli_kind": cli_kind })).unwrap();
    profile.claude_account = claude_account.map(str::to_string);
    profile
}

fn store(profiles: Vec<StoredAccount>) -> AccountsStore {
    AccountsStore { accounts: profiles, ..Default::default() }
}

fn registry(accounts: &[(&str, ClaudeAccountKind)]) -> ClaudeAccountsFile {
    ClaudeAccountsFile {
        accounts: accounts
            .iter()
            .map(|(id, kind)| ClaudeAccount {
                id: id.to_string(),
                label: format!("label {id}"),
                kind: *kind,
                created_at: "2026-10-09T00:00:00Z".into(),
                api: (*kind == ClaudeAccountKind::Api).then(ClaudeApiSettings::default),
                last_status: None,
            })
            .collect(),
        ..Default::default()
    }
}

fn claude_record(home: &Path) -> CutexSessionRecord {
    let mut record =
        CutexSessionRecord::new("cutex.a".into(), None, "host".into(), "/tmp".into(), None).unwrap();
    record.claude_session = Some(ClaudeSession {
        deployment: ClaudeDeployment {
            python: "/py".into(),
            adapter_source: "/adapter".into(),
            claude_cli: "/claude".into(),
            claude_home: home.to_path_buf(),
            tui_binary: "/tui".into(),
        },
        thread_id: "t".into(),
        state_dir: "/state".into(),
        model: "sonnet".into(),
        effort: None,
        instructions: String::new(),
        allow_write: false,
        allow_exec: false, write_explicit: false, exec_explicit: false,
        native_session_id: None,
        connection_id: None,
    });
    record
}

#[test]
fn ids_labels_and_keys_are_validated() {
    for id in ["ca-a", "ca-abc123", &format!("ca-{}", "a".repeat(32))] {
        validate_id(id).unwrap();
    }
    for id in ["default", "ca-", "ca-ABC", "ca-a/b", "ca-..", "x-abc", &format!("ca-{}", "a".repeat(33))] {
        assert!(validate_id(id).is_err(), "{id}");
    }
    let id = new_id();
    validate_id(&id).unwrap();
    assert_eq!(id.len(), 13);

    assert_eq!(validate_label("  Work  ").unwrap(), "Work");
    for label in ["", "   ", "Default", "a\nb", &"x".repeat(65)] {
        assert!(validate_label(label).is_err(), "{label:?}");
    }
    assert_eq!(validate_api_key(" sk-1 \n").unwrap(), "sk-1");
    for key in ["", "a b", &"k".repeat(MAX_API_KEY_BYTES + 1)] {
        assert!(validate_api_key(key).is_err());
    }
}

#[test]
fn api_settings_allow_only_non_secret_claude_code_env() {
    let mut api = ClaudeApiSettings {
        base_url: Some("https://api.deepseek.com/anthropic".into()),
        ..Default::default()
    };
    api.env.insert("ANTHROPIC_DEFAULT_SONNET_MODEL".into(), "deepseek-v4-pro".into());
    api.env.insert("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".into(), "1".into());
    api.validate().unwrap();
    for key in ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL", "CLAUDE_CONFIG_DIR", "PATH"] {
        let mut bad = api.clone();
        bad.env.insert(key.into(), "x".into());
        assert!(bad.validate().is_err(), "{key}");
    }
    // Nothing the adapter refuses for subscriptions may slip in as "extra".
    for key in CONFLICTING_ENV {
        assert!(!API_ENV_ALLOWED.contains(key), "{key}");
    }
    for url in ["ftp://x.example", "https://user:pw@x.example", "https://x.example/?k=1", "not a url"] {
        assert!(validate_base_url(url).is_err(), "{url}");
    }
    // Unknown fields in the stored settings are refused (no smuggled keys).
    assert!(serde_json::from_value::<ClaudeApiSettings>(json!({ "apiKey": "k" })).is_err());
    assert_eq!(ApiKeyEnv::parse("bearer"), Some(ApiKeyEnv::AuthToken));
    assert_eq!(serde_json::to_value(ApiKeyEnv::ApiKey).unwrap(), "ANTHROPIC_API_KEY");
}

#[test]
fn auth_status_keeps_only_non_secret_fields() {
    let status = parse_auth_status(&json!({
        "loggedIn": true,
        "authMethod": "claude.ai",
        "apiProvider": "firstParty",
        "email": "owner@example.com",
        "orgId": "org-1",
        "orgName": "Lab",
        "subscriptionType": "max",
        "accessToken": "secret-token",
        "refreshToken": "secret-refresh",
    }));
    assert!(status.logged_in);
    assert_eq!(status.subscription_type.as_deref(), Some("max"));
    let text = serde_json::to_string(&status).unwrap();
    assert!(!text.contains("secret"), "{text}");
    assert!(!parse_auth_status(&json!({ "loggedIn": false })).logged_in);
}

#[test]
fn profiles_resolve_to_their_account_and_sessions_never_change_account() {
    let _home = TestHome::new();
    let root = homes_root().unwrap();
    let reg = registry(&[("ca-work", ClaudeAccountKind::Subscription), ("ca-api", ClaudeAccountKind::Api)]);
    let profiles = store(vec![
        profile("shared", "claude", None),
        profile("work", "claude", Some("ca-work")),
        profile("work2", "claude", Some("ca-work")),
        profile("deepseek", "claude", Some("ca-api")),
        profile("gpt", "codex", None),
    ]);
    assert_eq!(profile_account_id(&profiles, None).as_deref(), Some("default"));
    assert_eq!(profile_account_id(&profiles, Some("shared")).as_deref(), Some("default"));
    assert_eq!(profile_account_id(&profiles, Some("id-work")).as_deref(), Some("ca-work"));
    assert_eq!(profile_account_id(&profiles, Some("gpt")), None);
    assert_eq!(profile_account_id(&profiles, Some("missing")), None);

    // Homes: registered → id; under the accounts root but unregistered →
    // removed:<id>; anything else (the deployment homes) → default.
    assert_eq!(account_id_for_home(&reg, &root.join("ca-work")), "ca-work");
    assert_eq!(account_id_for_home(&reg, &root.join("ca-gone")), "removed:ca-gone");
    assert_eq!(account_id_for_home(&reg, Path::new("/shared/login")), "default");

    let on_work = claude_record(&root.join("ca-work"));
    let on_default = claude_record(Path::new("/shared/login"));
    let on_removed = claude_record(&root.join("ca-gone"));
    // Same account: allowed (also between two profiles of that account).
    check_profile_change_with(&reg, &profiles, &on_work, Some("work2")).unwrap();
    check_profile_change_with(&reg, &profiles, &on_default, Some("shared")).unwrap();
    check_profile_change_with(&reg, &profiles, &on_default, None).unwrap();
    // Across accounts: refused, both ways.
    let mismatch = check_profile_change_with(&reg, &profiles, &on_default, Some("work")).unwrap_err();
    assert_eq!((mismatch.session_account.as_str(), mismatch.target_account.as_str()), ("default", "ca-work"));
    assert!(check_profile_change_with(&reg, &profiles, &on_work, Some("shared")).is_err());
    assert!(check_profile_change_with(&reg, &profiles, &on_work, None).is_err());
    assert!(check_profile_change_with(&reg, &profiles, &on_work, Some("deepseek")).is_err());
    // A removed account's session matches no profile.
    assert!(check_profile_change_with(&reg, &profiles, &on_removed, Some("shared")).is_err());
    // Non-Claude targets and non-Claude sessions are judged elsewhere.
    check_profile_change_with(&reg, &profiles, &on_work, Some("gpt")).unwrap();
    let mut codex = on_work.clone();
    codex.claude_session = None;
    check_profile_change_with(&reg, &profiles, &codex, Some("shared")).unwrap();

    let presented = session_account_json(&reg, &on_work).unwrap();
    assert_eq!(presented, json!({ "id": "ca-work", "label": "label ca-work", "kind": "subscription", "isDefault": false }));
    assert_eq!(session_account_json(&reg, &on_removed).unwrap()["kind"], "removed");
    assert_eq!(session_account_json(&reg, &on_default).unwrap()["isDefault"], true);
}

#[test]
fn accounts_are_added_updated_and_removed_without_leaking_keys() {
    let _home = TestHome::new();
    assert!(load().unwrap().accounts.is_empty());

    let work = add(NewClaudeAccount {
        label: "Work".into(),
        kind: ClaudeAccountKind::Subscription,
        api: None,
        api_key: None,
    })
    .unwrap();
    let home = account_home(&work.id).unwrap();
    assert!(home.is_dir(), "the account home is created owner-private");
    assert!(api_launch_config(&home).unwrap().is_none(), "subscription homes get no API config");
    // Labels are unique; subscription accounts take no API settings.
    assert!(add(NewClaudeAccount { label: "Work".into(), kind: ClaudeAccountKind::Subscription, api: None, api_key: None }).is_err());
    assert!(add(NewClaudeAccount {
        label: "X".into(),
        kind: ClaudeAccountKind::Subscription,
        api: None,
        api_key: Some("sk-x".into()),
    })
    .is_err());

    let api = add(NewClaudeAccount {
        label: "DeepSeek".into(),
        kind: ClaudeAccountKind::Api,
        api: Some(ClaudeApiSettings {
            base_url: Some("https://api.deepseek.com/anthropic".into()),
            ..Default::default()
        }),
        api_key: Some("sk-test-only-123".into()),
    })
    .unwrap();
    assert!(api_key_present(&api.id));
    let key_file = key_path(&api.id).unwrap();
    assert!(!key_file.starts_with(account_home(&api.id).unwrap()), "the key is outside the Claude home");
    let launch = api_launch_config(&account_home(&api.id).unwrap()).unwrap().unwrap();
    assert_eq!(launch["accountId"], json!(api.id));
    assert_eq!(launch["keyEnv"], "ANTHROPIC_API_KEY");
    assert_eq!(launch["baseUrl"], "https://api.deepseek.com/anthropic");
    assert!(!launch.to_string().contains("sk-test-only"), "the launch config names the key file only");

    // Registry and presentation never contain the key.
    let registry_text = std::fs::read_to_string(registry_path().unwrap()).unwrap();
    assert!(!registry_text.contains("sk-test-only"));
    let item = item_json(&load().unwrap(), &api.id, None).unwrap();
    assert_eq!(item["api"]["apiKeyPresent"], true);
    assert_eq!(item["status"]["loggedIn"], true);
    assert_eq!(item["status"]["apiProvider"], "anthropicCompatible");
    assert!(item["loginCommand"].is_null());
    assert!(!item.to_string().contains("sk-test-only"));
    let work_item = item_json(&load().unwrap(), &work.id, None).unwrap();
    assert!(work_item["loginCommand"].is_null() || work_item["loginCommand"]["cutex"].is_string());

    // Update: label, base URL clear, key replacement; API fields refused on subscriptions.
    let updated = update(
        &api.id,
        AccountPatch {
            label: Some("DS".into()),
            base_url: Change::Clear,
            api_key: Some("sk-test-only-456".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(updated.label, "DS");
    assert_eq!(updated.api.as_ref().unwrap().base_url, None);
    assert!(update(&work.id, AccountPatch { base_url: Change::Set("https://x.example".into()), ..Default::default() }).is_err());
    assert!(update(&work.id, AccountPatch { label: Some("DS".into()), ..Default::default() }).is_err());
    assert!(update(DEFAULT_ACCOUNT_ID, AccountPatch { label: Some("x".into()), ..Default::default() }).is_err());

    // Remove: default and unknown refused; a bound profile blocks removal.
    assert!(matches!(remove(DEFAULT_ACCOUNT_ID, false), Err(RemoveError::IsDefault)));
    assert!(matches!(remove("ca-nothere", false), Err(RemoveError::NotFound)));
    assert!(matches!(remove("../x", false), Err(RemoveError::NotFound)));
    let mut profiles = crate::profiles::settings_api::load_store().unwrap_or_default();
    profiles.accounts.push(profile("bound", "claude", Some(&work.id)));
    crate::profiles::store::save_store(&profiles).unwrap();
    match remove(&work.id, false) {
        Err(RemoveError::InUse(usage)) => assert_eq!(usage.profiles[0].name, "bound"),
        other => panic!("expected in use, got {other:?}"),
    }
    profiles.accounts.clear();
    crate::profiles::store::save_store(&profiles).unwrap();
    let removed = remove(&work.id, false).unwrap();
    assert_eq!(removed.kept_home.as_deref(), Some(home.as_path()), "the home is kept by default");
    assert!(home.is_dir());
    let removed_api = remove(&api.id, true).unwrap();
    assert!(removed_api.home_deleted && !key_file.exists(), "the key goes with the account");
    assert!(load().unwrap().accounts.is_empty());
}

#[test]
fn list_json_matches_the_exported_schema() {
    let _home = TestHome::new();
    add(NewClaudeAccount {
        label: "DeepSeek".into(),
        kind: ClaudeAccountKind::Api,
        api: Some(ClaudeApiSettings {
            base_url: Some("https://api.deepseek.com/anthropic".into()),
            env: [("ANTHROPIC_DEFAULT_SONNET_MODEL".to_string(), "deepseek-v4-pro".to_string())].into(),
            ..Default::default()
        }),
        api_key: None,
    })
    .unwrap();
    add(NewClaudeAccount { label: "Work".into(), kind: ClaudeAccountKind::Subscription, api: None, api_key: None })
        .unwrap();
    let list = list_json().unwrap();
    assert_eq!(list["accounts"][0]["id"], "default");
    assert_eq!(list["accounts"].as_array().unwrap().len(), 3);
    assert_eq!(list["accounts"][1]["status"]["loggedIn"], false, "an API account without a key is not ready");
    crate::management::v2::schema_export::assert_valid("claude-accounts", "accountList", &list);
    let registry = load().unwrap();
    let record = claude_record(&account_home(&registry.accounts[0].id).unwrap());
    crate::management::v2::schema_export::assert_valid(
        "claude-accounts",
        "sessionAccount",
        &session_account_json(&registry, &record).unwrap(),
    );
    // An API account without a key cannot launch.
    assert!(api_launch_config(&account_home(&registry.accounts[0].id).unwrap()).is_err());
}
