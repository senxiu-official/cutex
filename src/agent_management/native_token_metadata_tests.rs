// Synthetic native login files through the real review/execution/upgrade paths.
use super::*;
use crate::launch::stock::{StockBundle, VerifiedFile};
use crate::session::model::CutexSessionStore;
use std::path::{Path, PathBuf};

struct Fixture {
    path: PathBuf,
    auth: PathBuf,
    id: CutexSessionId,
    provider: AgentManagementProvider,
    _home: AutomaticStartHome,
}

fn private_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn write(path: &Path, value: &[u8]) {
    crate::config::atomic::write_private_bytes_atomic(path, value).unwrap();
}

fn bundle(home: &Path, bin: &Path) -> StockBundle {
    private_dir(home);
    private_dir(bin);
    let file = |name: &str, bytes: &[u8]| {
        let path = bin.join(name);
        write(&path, bytes);
        VerifiedFile { sha256: file_sha256(&path).unwrap(), path: path.canonicalize().unwrap() }
    };
    write(&home.join("config.toml"), b"model='gpt-5.6-sol'\n");
    StockBundle {
        version: 4, upstream_commit: "synthetic".into(), native_patch_commit: None,
        executable: file("codex-app-server", b"synthetic-server"), cli: None,
        code_mode_host: file("codex-code-mode-host", b"synthetic-host"),
        facade: file("cutex-mcp", b"synthetic-facade"), schema: file("schema.json", b"{}"),
        shared_config: VerifiedFile { path: home.join("config.toml").canonicalize().unwrap(),
            sha256: file_sha256(&home.join("config.toml")).unwrap() },
        launch_config_sha256: None,
    }
}

impl Fixture {
    fn new() -> Self {
        let home = AutomaticStartHome::new();
        let config = crate::config::paths::config_dir().unwrap();
        let profile_id = uuid::Uuid::new_v4().to_string();
        let profile = config.join("profiles").join(&profile_id);
        private_dir(&profile);
        let auth = profile.join("auth.json");
        write(&auth, &serde_json::to_vec(&crate::launch::chatgpt_auth_file::metadata_tests::auth()).unwrap());
        write(&profile.join("config.toml"), b"cutex_provider_mode='selected_profile_v2'\nmodel='gpt-5.6-sol'\nmodel_reasoning_effort='max'\n");
        write(&crate::config::paths::accounts_path().unwrap(), &serde_json::to_vec(&json!({
            "version":3,"active_account_id":null,"accounts":[{"id":profile_id,"name":"synthetic-metadata",
                "email":null,"plan_type":null,"last_used_at":null}]})).unwrap());
        let native_id = uuid::Uuid::new_v4().to_string();
        let native_home = crate::config::paths::runtime_dir().unwrap().join("light/agents").join(&native_id);
        private_dir(&native_home);
        let native_home = native_home.canonicalize().unwrap();
        let current_bundle = bundle(&native_home, &config.join("old-bin"));
        let manifest = native_home.join("bundle.json");
        write(&manifest, &serde_json::to_vec(&current_bundle).unwrap());
        private_dir(&native_home.join("sessions"));
        write(&native_home.join("sessions").join(format!("rollout-{native_id}.jsonl")),
            format!("{}\n",json!({"type":"session_meta","payload":{"id":native_id}})).as_bytes());
        let mut record = CutexSessionRecord::new(format!("cutex.{native_id}"), Some(native_id.clone()),
            crate::platform::host::current_host_name(), home.root.to_string_lossy().into_owned(),
            Some("synthetic-metadata".into())).unwrap();
        record.agent_enabled = true;
        record.registration_class = crate::agent_bus::model::AgentRegistrationClass::Persistent;
        record.formal_agent_name = Some("synthetic-metadata".into());
        record.permission_defaults = Some("danger-full-access".into());
        record.sandbox_mode = Some("danger-full-access".into());
        record.approval_policy = Some("never".into());
        record.explicit_launch = Some(ExplicitLaunchContract {
            version:4, migration_action_id:None, native_id, native_home: native_home.clone(),
            bundle_sha256:file_sha256(&manifest).unwrap(), bundle_manifest:manifest.canonicalize().unwrap(),
        });
        let id = CutexSessionId::new(record.cutex_session_id.clone()).unwrap();
        let mut sessions = CutexSessionStore::default();
        sessions.sessions.insert(record.cutex_session_id.clone(), record);
        let path = crate::session::store::cutex_sessions_path().unwrap();
        crate::session::store::save_cutex_session_store_to_path(&path,&sessions).unwrap();
        let selected_home = config.join("runtime/light/template");
        let selected = bundle(&selected_home,&config.join("new-bin"));
        let selected_manifest = selected_home.join("bundle.json");
        write(&selected_manifest,&serde_json::to_vec(&selected).unwrap());
        write(&config.join("runtime/light/deployment.json"),&serde_json::to_vec(&json!({
            "native_home":selected_home.canonicalize().unwrap(),"bundle_manifest":selected_manifest.canonicalize().unwrap()})).unwrap());
        let provider = AgentManagementProvider::open_default().unwrap();
        Self { path, auth, id, provider, _home:home }
    }

    fn change_auth(&self, change: impl FnOnce(&mut serde_json::Value)) {
        // Preserve the inode/custody, as native's selected-file refresh does.
        let mut auth = crate::launch::chatgpt_auth_file::metadata_tests::auth();
        change(&mut auth);
        std::fs::write(&self.auth, serde_json::to_vec(&auth).unwrap()).unwrap();
    }
}

#[derive(Default)]
struct BeforeSpawn { reached: bool }
impl StockRuntimeExecutor for BeforeSpawn {
    fn publication(&mut self, _: &StockRuntimeReceipt) -> anyhow::Result<StockPublication> {
        self.reached = true;
        anyhow::bail!("synthetic-before-spawn")
    }
    fn published_owner_absent(&mut self, _: &StockRuntimeReceipt) -> anyhow::Result<bool> { panic!("no process") }
    fn stop(&mut self, _: &CutexSessionRecord) -> anyhow::Result<()> { panic!("no process") }
    fn spawn(&mut self, _: &CutexSessionRecord, _: &StockBundle, _: &StockRuntimeReceipt)
        -> anyhow::Result<CutexAppServerRuntimeBinding> { panic!("no process") }
    fn connect(&mut self, _: &CutexSessionRecord, _: &StockRuntimeReceipt) -> anyhow::Result<()> { panic!("no process") }
    fn cleanup_owned(&mut self) -> anyhow::Result<()> { panic!("no process") }
    fn retain_owner(&mut self) { panic!("no process") }
}

#[test]
fn native_login_metadata_normal_online_review_and_execution_recheck() {
    let f = Fixture::new();
    let review = f.provider.review_stock_runtime(&f.path,&f.id,false).unwrap();
    f.change_auth(|value| { value["tokens"]["access_token"] = json!("synthetic-rotated"); });
    let mut runtime = BeforeSpawn::default();
    let error = f.provider.execute_stock_runtime(&HumanManagementPrincipal::authenticated(),&f.path,
        &AgentActionId::new("synthetic-metadata-online").unwrap(),&review,&mut runtime).unwrap_err();
    assert!(runtime.reached, "{error:#}");
    assert!(format!("{error:#}").contains("synthetic-before-spawn"));
    let auth: serde_json::Value = serde_json::from_slice(&std::fs::read(&f.auth).unwrap()).unwrap();
    assert_eq!(auth["tokens"]["login_owner_id"],"fixture-login");
    assert_eq!(auth["tokens"]["oauth_provenance"]["client_id"],"fixture-client");
}

#[test]
fn native_login_metadata_execution_still_rejects_auth_changed_after_review() {
    let f = Fixture::new();
    let review = f.provider.review_stock_runtime(&f.path,&f.id,false).unwrap();
    f.change_auth(|value| { value["tokens"]["oauth_provenance"]["issuer"] = json!(false); });
    let mut runtime = BeforeSpawn::default();
    let error = f.provider.execute_stock_runtime(&HumanManagementPrincipal::authenticated(),&f.path,
        &AgentActionId::new("synthetic-invalid-metadata").unwrap(),&review,&mut runtime).unwrap_err();
    assert!(!runtime.reached);
    assert!(format!("{error:#}").contains("invalid native OAuth provenance metadata"),"{error:#}");
}

#[test]
fn native_login_metadata_runtime_package_review_apply_preserves_auth() {
    let f = Fixture::new();
    let review = f.provider.review_runtime_package_upgrade(&f.path,&f.id).unwrap();
    let before = std::fs::read(&f.auth).unwrap();
    f.provider.apply_runtime_package_upgrade(&HumanManagementPrincipal::authenticated(),&f.path,
        &AgentActionId::new("synthetic-metadata-upgrade").unwrap(),&review).unwrap();
    assert_eq!(std::fs::read(&f.auth).unwrap(),before);
    f.provider.review_stock_runtime(&f.path,&f.id,false).unwrap();
}

#[test]
fn native_login_metadata_import_materialize_preserves_native_bytes() {
    let f = Fixture::new();
    let snapshot = crate::profiles::import::import_snapshot(f.auth.to_str().unwrap(), None).unwrap();
    let account = crate::profiles::model::StoredAccount::from_import("synthetic-copy".into(), &snapshot,
        crate::profiles::model::RuntimeConfig::Host);
    crate::profiles::materialize::materialize_imported_account_files(&account,&snapshot).unwrap();
    let files = crate::profiles::materialize::materialized_account_files(&account).unwrap();
    assert_eq!(std::fs::read(&files.auth_path).unwrap(),std::fs::read(&f.auth).unwrap());
    crate::launch::selected_profile::Config::parse("cutex_provider_mode='selected_profile_v2'\nmodel='gpt-5.6-sol'\nmodel_reasoning_effort='max'\n")
        .unwrap().review(&account.id,files.auth_path,None,None).unwrap();
}
