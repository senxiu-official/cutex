use super::*;

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cutex-kimi-{name}-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn binding(state_dir: &Path, options: KimiOptions) -> DshSession {
    DshSession {
        flavor: AcpFlavor::Kimi,
        kimi: Some(options),
        thread_id: uuid::Uuid::new_v4().to_string(),
        state_dir: state_dir.into(),
        node: "/node".into(),
        adapter: "/adapter/src/server.mjs".into(),
        dsh_cli: "/install/node_modules/@moonshot-ai/kimi-code/dist/main.mjs".into(),
        patch: state_dir.join("home/kimi/config.toml"),
        install_root: "/install".into(),
        native_session_id: None,
        deployment_version: PINNED_KIMI_VERSION.into(),
        model: "kimi-for-coding".into(),
        reasoning_effort: None,
        api_key_env: String::new(),
        spike: false,
    }
}

fn record(cwd: &Path, session: DshSession) -> CutexSessionRecord {
    let mut record = CutexSessionRecord::new(format!("cutex.{}", uuid::Uuid::new_v4()), None, "host".into(), cwd.to_string_lossy().into_owned(), None).unwrap();
    record.runtime_backend = CutexSessionRuntimeBackend::Kimi;
    record.dsh_session = Some(session);
    record
}

fn env_options(name: &str) -> KimiOptions {
    KimiOptions { credential: KimiCredentialSource::Env { env_name: name.into(), base_url: "https://api.kimi.com/coding/v1".into() }, models: Vec::new() }
}

#[test]
fn dsh_records_stay_byte_identical_and_kimi_bindings_round_trip() {
    let dsh: DshSession = serde_json::from_value(json!({
        "thread_id": "t", "state_dir": "/s", "node": "/n", "adapter": "/a", "dsh_cli": "/d", "patch": "/s/profile.patch.yml",
        "install_root": "/i", "spike": false,
    }))
    .unwrap();
    assert_eq!(dsh.flavor, AcpFlavor::Dsh);
    let wire = serde_json::to_value(&dsh).unwrap();
    assert!(wire.get("flavor").is_none() && wire.get("kimi").is_none(), "{wire}");
    let state = temp("serde");
    let kimi = binding(&state, env_options("KIMI_TEST_KEY"));
    let wire = serde_json::to_value(&kimi).unwrap();
    assert_eq!(wire["flavor"], "kimi");
    assert_eq!(wire["kimi"]["credential"], json!({"source": "env", "env_name": "KIMI_TEST_KEY", "base_url": "https://api.kimi.com/coding/v1"}));
    assert_eq!(serde_json::from_value::<DshSession>(wire).unwrap(), kimi);
    let profile = KimiOptions { credential: KimiCredentialSource::Profile, models: Vec::new() };
    assert_eq!(serde_json::to_value(&profile).unwrap(), json!({"credential": {"source": "profile"}}));
    // The record's runtime backend serializes as "kimi" and parses back.
    let record = record(&state, kimi);
    assert_eq!(serde_json::to_value(&record).unwrap()["runtime_backend"], "kimi");
    assert_eq!(crate::session::model::parse_cutex_session_runtime_backend("kimi").unwrap(), CutexSessionRuntimeBackend::Kimi);
    assert!(record.is_owned_session() || record.agent_enabled, "a Kimi agent record is a cutex-owned runtime");
    std::fs::remove_dir_all(state).unwrap();
}

#[test]
fn permission_settings_map_onto_kimi_modes_and_rules() {
    let state = temp("perm");
    let mut record = record(&state, binding(&state, env_options("KIMI_TEST_KEY")));
    let case = |record: &mut CutexSessionRecord, sandbox: Option<&str>, approval: Option<&str>| {
        record.sandbox_mode = sandbox.map(Into::into);
        record.approval_policy = approval.map(Into::into);
        permission_mapping(record)
    };
    let cutex = ("allow", "mcp__cutex__*");
    assert_eq!(case(&mut record, None, None), ("default", "ask", vec![cutex]));
    assert_eq!(case(&mut record, Some("workspace-write"), Some("on-request")), ("default", "ask", vec![cutex]));
    assert_eq!(case(&mut record, Some("danger-full-access"), Some("never")), ("auto", "never", vec![cutex]));
    assert_eq!(case(&mut record, Some("full-access"), Some("on-request")), ("yolo", "ask", vec![cutex]));
    assert_eq!(case(&mut record, Some("read-only"), None), ("default", "ask", vec![cutex, ("deny", "Write"), ("deny", "Edit")]));
    assert_eq!(case(&mut record, Some("workspace-write"), Some("never")), ("default", "never", vec![cutex]));
    // The permission alias alone (agent_create's full access) means never-ask.
    record.sandbox_mode = None;
    record.approval_policy = None;
    record.permission_defaults = Some("full-access".into());
    assert_eq!(permission_mapping(&record).0, "auto");
    std::fs::remove_dir_all(state).unwrap();
}

/// The read-only hook command refuses (exit 2, a message on stderr) in the
/// shell Kimi spawns hooks with: `sh` on Unix, `cmd.exe` on Windows.
#[test]
fn the_read_only_hook_command_exits_2_with_a_message() {
    #[cfg(unix)]
    let output = std::process::Command::new("/bin/sh").arg("-c").arg(READ_ONLY_HOOK_COMMAND).output().unwrap();
    // As Node's `shell: true` runs it: `cmd.exe /d /s /c "<command>"`.
    #[cfg(windows)]
    let output = {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("cmd").args(["/d", "/s", "/c"]).raw_arg(format!("\"{READ_ONLY_HOOK_COMMAND}\"")).output().unwrap()
    };
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("read-only"), "{output:?}");
    assert!(!READ_ONLY_HOOK_COMMAND.contains(';') && !READ_ONLY_HOOK_COMMAND.contains('|'));
}

#[test]
fn a_stray_empty_git_above_the_cwd_is_refused_but_real_repositories_are_not() {
    let base = temp("root");
    // A home folder with an empty .git (no HEAD), as on this host.
    std::fs::create_dir_all(base.join(".git")).unwrap();
    std::fs::write(base.join("AGENTS.md"), "owner's private file").unwrap();
    let agent = base.join("agents/a");
    std::fs::create_dir_all(&agent).unwrap();
    assert_eq!(project_root(&agent), base);
    let error = check_project_root(&agent).unwrap_err().to_string();
    assert!(error.contains("git init"), "{error}");
    // The agent's folder as its own repository: Kimi's root is the cwd.
    std::fs::create_dir_all(agent.join(".git")).unwrap();
    check_project_root(&agent).unwrap();
    assert!(project_instruction_files(&agent).is_empty());
    // A real repository above the cwd (HEAD present, or a worktree's .git file) is the project, as in Codex.
    let repo = temp("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    let sub = repo.join("crate/src");
    std::fs::create_dir_all(sub.join(".kimi-code")).unwrap();
    std::fs::write(repo.join("AGENTS.md"), "x").unwrap();
    std::fs::write(repo.join("crate/agents.md"), "x").unwrap();
    std::fs::write(sub.join(".kimi-code/AGENTS.md"), "x").unwrap();
    std::fs::write(sub.join("CLAUDE.md"), "not read by Kimi").unwrap();
    check_project_root(&sub).unwrap();
    // Compared as the files they name: on a case-insensitive filesystem
    // (Windows, macOS) `crate/AGENTS.md` is the file `crate/agents.md`.
    let real = |paths: Vec<PathBuf>| -> Vec<PathBuf> { paths.into_iter().map(|path| std::fs::canonicalize(path).unwrap()).collect() };
    assert_eq!(real(project_instruction_files(&sub)), real(vec![repo.join("AGENTS.md"), repo.join("crate").join("agents.md"), sub.join(".kimi-code").join("AGENTS.md")]));
    let worktree = temp("worktree");
    std::fs::write(worktree.join(".git"), "gitdir: /elsewhere\n").unwrap();
    std::fs::create_dir_all(worktree.join("a")).unwrap();
    check_project_root(&worktree.join("a")).unwrap();
    for dir in [base, repo, worktree] {
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn config_declares_provider_models_switches_and_rules_without_the_key() {
    let _home = crate::profiles::lifecycle::tests::TestHome::new();
    let state = temp("config");
    let cwd = temp("config-cwd");
    std::fs::create_dir_all(cwd.join(".git")).unwrap();
    let env = "CUTEX_KIMI_UNIT_TEST_KEY_A";
    unsafe { std::env::set_var(env, "sk-fake-kimi-unit") };
    let mut options = env_options(env);
    options.models.push(KimiModel { alias: "k3".into(), model: "k3".into(), max_context_size: 1_048_576, capabilities: vec!["thinking".into(), "image_in".into()],
        support_efforts: vec!["low".into(), "high".into()], default_effort: Some("high".into()), display_name: Some("K3".into()) });
    let mut record = record(&cwd, binding(&state, options));
    record.sandbox_mode = Some("read-only".into());
    let parts = write_home(&record).unwrap();
    unsafe { std::env::remove_var(env) };
    assert_eq!(parts.secret_env, Some((env.to_string(), "sk-fake-kimi-unit".to_string())));
    assert_eq!((parts.mode, parts.approval_policy, parts.model.as_str()), ("default", "ask", "kimi-for-coding"));
    assert_eq!(parts.kimi_home, state.join("home/kimi"));
    assert_eq!(parts.image_models, ["k3", "k3-256k", "kimi-for-coding", "kimi-for-coding-highspeed"]);
    let text = std::fs::read_to_string(state.join("home/kimi/config.toml")).unwrap();
    assert!(!text.contains("sk-fake"), "{text}");
    let config: toml::Table = toml::from_str(&text).unwrap();
    assert_eq!(config["default_model"].as_str(), Some("kimi-for-coding"));
    assert_eq!(config["telemetry"].as_bool(), Some(false));
    assert_eq!(config["watch"]["enabled"].as_bool(), Some(false));
    let provider = &config["providers"][API_KEY_PROVIDER];
    assert_eq!((provider["type"].as_str(), provider["api_key_env"].as_str(), provider["base_url"].as_str()), (Some("kimi"), Some(KIMI_KEY_ENV), Some("https://api.kimi.com/coding/v1")));
    assert_eq!(config["models"]["kimi-for-coding"]["provider"].as_str(), Some(API_KEY_PROVIDER));
    assert_eq!(config["models"]["kimi-for-coding"]["max_context_size"].as_integer(), Some(1_048_576));
    assert_eq!(config["models"]["k3"]["max_context_size"].as_integer(), Some(1_048_576), "the agent's own entry wins");
    assert_eq!(config["models"]["k3"]["support_efforts"].as_array().unwrap().len(), 2);
    let disabled: Vec<_> = config["tools"]["disabled"].as_array().unwrap().iter().filter_map(toml::Value::as_str).collect();
    assert_eq!(disabled, DISABLED_TOOLS);
    let rules: Vec<_> = config["permission"]["rules"].as_array().unwrap().iter()
        .map(|rule| (rule["decision"].as_str().unwrap(), rule["pattern"].as_str().unwrap())).collect();
    assert_eq!(rules, vec![("allow", "mcp__cutex__*"), ("deny", "Write"), ("deny", "Edit")]);
    // Read-only is enforced by Kimi's PreToolUse hook (2.3.3 never applies the rules above).
    assert!(parts.read_only && parts.redaction.is_none());
    let hooks = config["hooks"].as_array().unwrap();
    assert_eq!(hooks.len(), 1);
    assert_eq!((hooks[0]["event"].as_str(), hooks[0]["command"].as_str()), (Some("PreToolUse"), Some(READ_ONLY_HOOK_COMMAND)));
    let matcher = hooks[0]["matcher"].as_str().unwrap();
    assert!(matcher.starts_with("^(?!(Read|Grep|Glob|") && matcher.ends_with("|mcp__cutex__.*)$)"), "{matcher}");
    // cutex's own instructions go to Kimi's private AGENTS.md; the user's real one never leaks in through HOME.
    let agents = std::fs::read_to_string(state.join("home/kimi/AGENTS.md")).unwrap_or_default();
    assert!(agents.contains("# File references"), "{agents}");
    // The env credential refuses cleanly without its variable (no fallback).
    let error = format!("{:#}", write_home(&record).err().unwrap());
    assert!(error.contains("credential_missing") && error.contains(env), "{error}");
    for dir in [state, cwd] {
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn a_profile_api_key_launch_uses_the_profile_resolver_and_the_stored_key() {
    let _home = crate::profiles::lifecycle::tests::TestHome::new();
    let created = crate::management::v2::profiles_api::create_from_body(
        br#"{"name":"Kimi K","kind":"apiKey","preset":"kimi-code","baseUrl":"https://api.kimi.com/coding/v1","model":"kimi-for-coding","apiKey":"sk-fake-r266-kimi-a"}"#,
    )
    .unwrap();
    let id = created["profile"]["id"].as_str().unwrap().to_string();
    crate::profiles::runtime_variants::add_variant(&id, "kimi", None, Default::default()).unwrap();
    // `plan` refuses while no component is installed in this test process;
    // the frozen binding it would write is a stored-key Kimi binding.
    let bound = crate::profiles::runtime_variants::ProfileLaunchBinding {
        profile_id: id.clone(), variant_id: "kimi".into(), runtime: "kimi".into(),
        credential: crate::profiles::runtime_variants::BoundCredential { source: "stored".into(), env_name: None, version: None, kimi_account_id: None },
        model: Some("kimi-for-coding".into()), reasoning_effort: None, runtime_package_version: Some(PINNED_KIMI_VERSION.into()), bound_at: "now".into(),
    };
    let state = temp("profile");
    let cwd = temp("profile-cwd");
    std::fs::create_dir_all(cwd.join(".git")).unwrap();
    let mut record = record(&cwd, binding(&state, KimiOptions { credential: KimiCredentialSource::Profile, models: Vec::new() }));
    record.profile_binding = Some(bound);
    let parts = write_home(&record).unwrap();
    assert_eq!(parts.secret_env, Some((KIMI_KEY_ENV.to_string(), "sk-fake-r266-kimi-a".to_string())));
    assert_eq!(credential::for_redaction(&record).as_deref(), Some("sk-fake-r266-kimi-a"));
    assert_eq!(crate::runtime::dsh::credential_for_redaction(&record).as_deref(), Some("sk-fake-r266-kimi-a"));
    let text = std::fs::read_to_string(state.join("home/kimi/config.toml")).unwrap();
    assert!(!text.contains("sk-fake") && text.contains(KIMI_KEY_ENV), "{text}");
    // A Profile agent without its binding refuses (no env fallback).
    record.profile_binding = None;
    assert!(format!("{:#}", write_home(&record).err().unwrap()).contains("invalid_binding"));
    for dir in [state, cwd] {
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn schema_and_contract_agree_and_declare_what_the_adapter_sends() {
    let schema = native_schema();
    assert_eq!(schema["sha256"], contract_sha256());
    assert_eq!(schema["capabilities"]["features"]["turnSteer"], false);
    assert_eq!(schema["capabilities"]["features"]["osSandbox"], false);
    let contract: Value = serde_json::from_slice(include_bytes!("kimi-contract.json")).unwrap();
    assert_eq!(contract["runtimeBackend"], "kimi");
    assert_eq!(contract["nativeProtocol"]["productVersion"], PINNED_KIMI_VERSION);
    let declared: Vec<&str> = contract["notifications"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    let unique: std::collections::BTreeSet<_> = declared.iter().collect();
    assert_eq!(unique.len(), declared.len());
    // Every notification the adapter broadcasts is declared.
    let source = concat!(include_str!("../../components/kimi-runtime/src/runtime.mjs"), include_str!("../../components/kimi-runtime/src/projection.mjs"));
    for method in ["turn/started", "turn/completed", "item/started", "item/completed", "item/updated", "item/agentMessage/delta", "item/reasoning/textDelta",
        "turn/plan/updated", "thread/tokenUsage/updated", "kimi/launchSelection"] {
        assert!(source.contains(&format!("'{method}'")), "{method} not sent");
        assert!(declared.contains(&method), "{method} not declared");
    }
    // The daemon drives the adapter through the shared ACP control methods.
    let requests: Vec<&str> = contract["requests"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    for method in ["dsh/runtime/status", "dsh/input/submit", "dsh/input/status", "dsh/server/shutdown"] {
        assert!(requests.contains(&method));
        assert!(include_str!("../../components/kimi-runtime/src/server.mjs").contains(&format!("'{method}'")), "{method} not served");
    }
}

#[test]
fn catalog_and_node_requirements() {
    // From cutex's documented model facts (profiles::known_models).
    let coding = catalog_model("kimi-for-coding");
    assert_eq!(coding.max_context_size, 1_048_576);
    assert!(coding.image_input() && coding.capabilities.contains(&"thinking".to_string()));
    assert_eq!((coding.support_efforts.len(), coding.default_effort.as_deref()), (3, Some("max")));
    assert_eq!(catalog_model("k3").default_effort.as_deref(), Some("high"));
    let unknown = catalog_model("some-relay-model");
    assert_eq!((unknown.max_context_size, unknown.capabilities.clone()), (262_144, vec!["tool_use".to_string()]));
    // An API-key launch declares the endpoint's models and the agent's own.
    let moonshot: Vec<_> = api_key_models(None, Some("https://api.moonshot.ai/v1"), "my-model", &[]).into_iter().map(|model| model.alias).collect();
    assert_eq!(moonshot, ["kimi-k3", "kimi-k2.7-code", "kimi-k2.7-code-highspeed", "kimi-k2.6", "my-model"]);
    let coding: Vec<_> = api_key_models(None, Some("https://api.kimi.com/coding/v1"), "k3", &[]).into_iter().map(|model| model.alias).collect();
    assert_eq!(coding, KIMI_CODE_MODELS);
    // Profile launches (B8): the family follows the provider id, so a relay
    // URL that names neither keeps the documented models.
    use crate::profiles::kimi::ModelFamily;
    let relay = Some("https://relay.example/v1");
    let coding: Vec<_> = api_key_models(Some(ModelFamily::KimiCode), relay, "k3", &[]).into_iter().map(|model| model.alias).collect();
    assert_eq!(coding, KIMI_CODE_MODELS);
    let moonshot: Vec<_> = api_key_models(Some(ModelFamily::Moonshot), relay, "my-model", &[]).into_iter().map(|model| model.alias).collect();
    assert_eq!(moonshot, ["kimi-k3", "kimi-k2.7-code", "kimi-k2.7-code-highspeed", "kimi-k2.6", "my-model"]);
    // ... and wins over misleading URL text.
    let moonshot: Vec<_> = api_key_models(Some(ModelFamily::Moonshot), Some("https://relay.example/coding/v1"), "m", &[]).into_iter().map(|model| model.alias).collect();
    assert_eq!(moonshot.len(), 5);
    assert_eq!(api_key_models(None, relay, "x", &[]).len(), 1);
    assert!(node_version_ready("v22.19.0") && node_version_ready("v24.22.1") && node_version_ready("22.23.2"));
    assert!(!node_version_ready("v22.18.9") && !node_version_ready("v20.0.0") && !node_version_ready("v22.19.0-rc.1"));
}

#[test]
fn launch_options_are_validated() {
    validate_options(&env_options("KIMI_API_KEY")).unwrap();
    assert!(validate_options(&env_options("lower")).is_err());
    assert!(validate_options(&env_options(KIMI_KEY_ENV)).is_err(), "the child's carrier name is reserved");
    let plain = KimiOptions { credential: KimiCredentialSource::Env { env_name: "K".into(), base_url: "http://example.com".into() }, models: Vec::new() };
    assert!(validate_options(&plain).is_err());
    let loopback = KimiOptions { credential: KimiCredentialSource::Env { env_name: "K".into(), base_url: "http://127.0.0.1:9/v1".into() }, models: Vec::new() };
    assert_eq!(validate_options(&loopback).is_ok(), cfg!(debug_assertions));
}

#[test]
fn stop_control_survives_missing_launch_dependencies() {
    let state = temp("control");
    let record = record(&state, binding(&state.join("missing"), env_options("KIMI_TEST_KEY")));
    require_control(&record).unwrap();
    crate::runtime::dsh::require_control(&record).unwrap();
    assert!(require_session(&record).unwrap_err().to_string().contains("runtime file missing"));
    std::fs::remove_dir_all(state).unwrap();
}

#[test]
fn a_cutex_scratch_workspace_is_pinned_as_kimis_project_but_owner_folders_are_not() {
    let base = temp("scratch-pin");
    // The cutex home lives under a folder with a stray empty .git (as ~/.git on this host).
    std::fs::create_dir_all(base.join(".git")).unwrap();
    let config = base.join(".cutex");
    let scratch = config.join("agents-scratch").join("20261027-3fc74434");
    let chat = config.join("chats").join("20261027-0123abcd");
    let owner = config.join("agents-scratch").join("my-folder");
    let other = base.join("work/project");
    for dir in [&scratch, &chat, &owner, &other] {
        std::fs::create_dir_all(dir).unwrap();
    }
    for dir in [&scratch, &chat] {
        assert!(check_project_root(dir).is_err());
        pin_scratch_project_root_in(&config, dir).unwrap();
        assert!(dir.join(".git").is_dir());
        assert_eq!(project_root(dir), *dir);
        check_project_root(dir).unwrap();
        // Idempotent: the next start finds its own root.
        pin_scratch_project_root_in(&config, dir).unwrap();
    }
    // Not a cutex-allocated name, or not under the cutex home: the owner's folder, refused as before.
    for dir in [&owner, &other] {
        pin_scratch_project_root_in(&config, dir).unwrap();
        assert!(!dir.join(".git").exists());
        assert!(check_project_root(dir).is_err());
    }
    std::fs::remove_dir_all(base).unwrap();
}
