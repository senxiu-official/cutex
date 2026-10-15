//! Run only through selected_profile_private.py: private tmpfs root + no network.
#![cfg(target_os = "linux")]
use cutex::launch::selected_profile::{Config, Projection, Route};

// Fixture profile ids (no real profile).
const ALPHA_ID: &str = "00000000-0000-4000-8000-00000000000a";
const BETA_ID: &str = "00000000-0000-4000-8000-00000000000b";
const GAMMA_ID: &str = "00000000-0000-4000-8000-00000000000c";
use serde_json::{json, Value};
use std::io::{BufRead, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn write(path: &Path, bytes: &[u8]) {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    f.write_all(bytes).unwrap();
}
fn directory(path: &Path) {
    std::fs::create_dir(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}
fn auth(account: &str) -> Value {
    use base64::Engine;
    let claims = json!({"sub":"dummy-user", "email":format!("{account}@example.invalid"),
        "https://api.openai.com/auth":{"chatgpt_user_id":format!("user-{account}"),"chatgpt_account_id":account,"chatgpt_plan_type":"plus"}});
    let jwt = format!(
        "e30.{}.dummy",
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).unwrap())
    );
    json!({"auth_mode":"chatgpt","OPENAI_API_KEY":null,"tokens":{"id_token":jwt,"access_token":"dummy-access","refresh_token":"dummy-refresh","account_id":account},"last_refresh":null})
}
fn config(extra: &str) -> Config {
    Config::parse(&format!(
        "cutex_provider_mode='selected_profile_v2'\ncli_auth_credentials_store='file'\n{extra}"
    ))
    .unwrap()
}
struct Native {
    child: std::process::Child,
    rx: std::sync::mpsc::Receiver<Value>,
    seq: u64,
}
impl Drop for Native {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Native {
    fn new(home: &Path, projection: &Projection) -> Self {
        let exe = std::env::var("SELECTED_NATIVE_SERVER").unwrap();
        assert_eq!(
            cutex::agent_management::file_sha256(Path::new(&exe))
                .unwrap()
                .as_str(),
            cutex::launch::stock::S6E_EXECUTABLE_SHA256
        );
        let mut child = Command::new(exe)
            .env_clear()
            .env("HOME", "/tmp/home")
            .env("CODEX_HOME", home)
            .env("PATH", "/usr/bin:/bin")
            .env("RUST_LOG", "off")
            .args(projection.native_args(true).unwrap())
            .arg("--disable-plugin-startup-tasks-for-tests")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(v) = serde_json::from_str(&line) {
                    if tx.send(v).is_err() {
                        break;
                    }
                }
            }
        });
        let mut p = Self { child, rx, seq: 0 };
        p.rpc("initialize",json!({"clientInfo":{"name":"selected-private","version":"1"},"capabilities":{"experimentalApi":true}}));
        writeln!(
            p.child.stdin.as_mut().unwrap(),
            "{}",
            json!({"method":"initialized"})
        )
        .unwrap();
        p
    }
    fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.seq += 1;
        writeln!(
            self.child.stdin.as_mut().unwrap(),
            "{}",
            json!({"id":self.seq,"method":method,"params":params})
        )
        .unwrap();
        loop {
            let v = self
                .rx
                .recv_timeout(std::time::Duration::from_secs(20))
                .expect("bounded native RPC deadline");
            if v["id"].as_u64() == Some(self.seq) {
                assert!(v.get("error").is_none(), "native protocol refusal");
                return v["result"].clone();
            }
            assert_ne!(v["method"].as_str(), Some("turn/started"));
        }
    }
}

#[test]
#[ignore = "requires isolated selected_profile_private.py"]
fn selected_private_files_protocol_and_secret_boundary() {
    assert_eq!(std::env::var("SELECTED_PRIVATE_ROOT").unwrap(), "/tmp");
    let home = Path::new("/tmp/home");
    directory(home);
    let native_home = home.join("native");
    directory(&native_home);
    write(&native_home.join("config.toml"),b"cli_auth_credentials_store='file'\ncheck_for_update_on_startup=false\nchatgpt_base_url='http://127.0.0.1:9'\nmodel='private-no-turn'\n");
    write(&native_home.join("auth.json"), b"default-must-not-be-read");
    let mut projections = Vec::new();
    let model = "gpt-5.6-sol".to_string();
    let max = "max".to_string();
    for (name, id) in [
        ("a", ALPHA_ID),
        ("b", BETA_ID),
    ] {
        let dir = PathBuf::from(format!("/tmp/{name}"));
        directory(&dir);
        let path = dir.join("auth.json");
        write(&path, &serde_json::to_vec(&auth(name)).unwrap());
        let (p, m, r) =
            config("[shell_environment_policy]\nexclude=['CODEX_AUTH_FILE','CODEX_CONFIG_FILE']\n")
                .review(id, path.clone(), Some(&model), Some(&max))
                .unwrap();
        assert_eq!(m, model);
        assert_eq!(r.as_deref(), Some("max"));
        p.validate().unwrap();
        assert!(!p
            .native_args(false)
            .unwrap()
            .iter()
            .any(|a| a == "--auth-file"));
        let mut native = Native::new(&native_home, &p);
        let account = native.rpc("account/read", json!({"refreshToken":false}));
        assert_eq!(
            account["account"]["email"],
            format!("{name}@example.invalid")
        );
        projections.push((p, native));
    }
    assert_ne!(projections[0].0.auth.account, projections[1].0.auth.account);
    // Actual inode replacement, same account. Native's refresh HTTP mechanics
    // are reused from its accepted independent-auth test, not fabricated here.
    let a = &projections[0].0;
    let temp = a.auth.path.with_file_name("refresh.tmp");
    let mut refreshed = auth("a");
    refreshed["tokens"]["access_token"] = "dummy-refreshed".into();
    write(&temp, &serde_json::to_vec(&refreshed).unwrap());
    std::fs::rename(&temp, &a.auth.path).unwrap();
    a.validate().unwrap();
    let saved = serde_json::to_vec(a).unwrap();
    assert!(!String::from_utf8(saved).unwrap().contains("dummy-access"));
    write(&temp, &serde_json::to_vec(&auth("foreign")).unwrap());
    std::fs::rename(&temp, &a.auth.path).unwrap();
    assert!(a.validate().is_err());
    let b = &projections[1].0;
    let moved = b.auth.path.with_file_name("held-auth.json");
    std::fs::rename(&b.auth.path, &moved).unwrap();
    assert!(b.validate().is_err());
    std::os::unix::fs::symlink(&moved, &b.auth.path).unwrap();
    assert!(b.validate().is_err());
    std::fs::remove_file(&b.auth.path).unwrap();
    std::fs::rename(&moved, &b.auth.path).unwrap();
    b.validate().unwrap();
    assert_eq!(
        std::fs::read(native_home.join("auth.json")).unwrap(),
        b"default-must-not-be-read"
    );
    drop(projections);

    // Exercise actual account/default-profile lookup and durable overrides,
    // not just the standalone parser. This is private configuration, not a
    // manually created production session/store or authentication grant.
    std::env::set_var("HOME", home);
    let config_root = home.join(".cutex");
    directory(&config_root);
    let profile_root = config_root.join("profiles");
    directory(&profile_root);
    write(
        &config_root.join("config.json"),
        br#"{"default_profile":"alpha"}"#,
    );
    let mut accounts = Vec::new();
    for (name, id) in [
        ("alpha", ALPHA_ID),
        ("beta", BETA_ID),
    ] {
        let p = profile_root.join(id);
        directory(&p);
        write(
            &p.join("auth.json"),
            &serde_json::to_vec(&auth(name)).unwrap(),
        );
        write(&p.join("config.toml"),b"cutex_provider_mode='selected_profile_v2'\ncli_auth_credentials_store='file'\nmodel='gpt-5.6-terra'\nmodel_reasoning_effort='low'\n[tui]\nstatus_line=['custom:bon-voyage','custom:profile','model-with-reasoning']\nstatus_line_use_colors=true\n");
        write(&p.join("custom-status-items.json"), br##"{"items":[{"id":"custom:bon-voyage","title":"Bon voyage","source":{"kind":"static","value":"Bon voyage !"},"style":{"fg":"#F6A3C8","bold":true}},{"id":"custom:profile","title":"Profile","source":{"kind":"launch_profile"},"style":{"fg":"#FFFFFF","bold":true}}]}"##);
        accounts
            .push(json!({"id":id,"name":name,"email":null,"plan_type":null,"last_used_at":null}));
    }
    write(
        &config_root.join("accounts.json"),
        &serde_json::to_vec(&json!({"version":3,"accounts":accounts,"active_account_id":null}))
            .unwrap(),
    );
    let mut record = cutex::session::model::CutexSessionRecord::new(
        "cutex.private-selected".into(),
        Some(uuid::Uuid::new_v4().to_string()),
        "private".into(),
        "/tmp".into(),
        None,
    )
    .unwrap();
    record.model_defaults = Some("gpt-5.6-sol".into());
    record.reasoning_defaults = Some("max".into());
    record.permission_defaults = Some(":read-only".into());
    record.approval_policy = Some("never".into());
    let inherited = cutex::launch::stock::current_configuration(&record).unwrap();
    assert!(inherited.inherited);
    let status = inherited
        .selected_projection
        .as_ref()
        .unwrap()
        .status
        .as_ref()
        .unwrap();
    assert_eq!(status.payload.items[1].text, "alpha");
    assert_eq!(status.payload.items[0].style.fg.as_deref(), Some("#F6A3C8"));
    status.materialize().unwrap();
    assert_eq!(
        inherited.profile_id,
        ALPHA_ID
    );
    assert_eq!(
        (
            &*inherited.model,
            inherited.reasoning.as_deref(),
            &*inherited.sandbox,
            &*inherited.approval
        ),
        ("gpt-5.6-sol", Some("max"), "read-only", "never")
    );
    record.profile = Some("beta".into());
    record.permission_defaults = Some(":danger-full-access".into());
    let explicit = cutex::launch::stock::current_configuration(&record).unwrap();
    assert!(!explicit.inherited);
    assert_eq!(explicit.profile_id, BETA_ID);
    assert_eq!(explicit.sandbox, "danger-full-access");
    assert_ne!(
        inherited.selected_projection.as_ref().unwrap().auth.account,
        explicit.selected_projection.as_ref().unwrap().auth.account
    );
    record.reasoning_defaults = Some("guessed".into());
    assert!(cutex::launch::stock::current_configuration(&record).is_err());

    let dir = Path::new("/tmp/glm");
    directory(dir);
    let path = dir.join("auth.json");
    write(&path, br#"{"OPENAI_API_KEY":"synthetic-private-key"}"#);
    let catalog = dir.join("models.json");
    write(
        &catalog,
        br#"{"models":[{"slug":"glm-5.3","supported_reasoning_levels":[{"effort":"max"}]}]}"#,
    );
    let text=format!("model_provider='GLM'\nmodel_catalog_json='{}'\n[memories]\ngenerate_memories=false\nuse_memories=true\n[plugins.'sample@debug']\nenabled=true\n[model_providers.GLM]\nname='GLM'\nbase_url='https://relay.example/v1'\nwire_api='responses'\nrequires_openai_auth=false\nenv_key='OPENAI_API_KEY'\n",catalog.display());
    let (glm, _, _) = config(&text)
        .review(GAMMA_ID, path.clone(), Some(&"glm-5.3".into()), Some(&max))
        .unwrap();
    assert_eq!(glm.route, Route::ApiKey);
    let key = glm.secret().unwrap().unwrap();
    // Independent child -> real loopback HTTP oracle. This verifies secret
    // materialization, NOT native provider HTTP or real GLM acceptance.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let oracle = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut bytes = [0; 4096];
        let n = socket.read(&mut bytes).unwrap();
        let header = String::from_utf8_lossy(&bytes[..n]);
        let ok = header.contains("Authorization: Bearer synthetic-private-key");
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
        ok
    });
    let mut cmd = Command::new("/usr/bin/python3");
    cmd.env_clear().args(["-c","import os,urllib.request; r=urllib.request.Request('http://127.0.0.1:'+os.environ['PRIVATE_PORT']+'/'); r.add_header('Authorization','Bearer '+os.environ['OPENAI_API_KEY']); urllib.request.urlopen(r,timeout=5).close()"])
        .env("PRIVATE_PORT",port.to_string()).stdout(Stdio::null()).stderr(Stdio::null());
    key.apply(&mut cmd);
    assert!(cmd.status().unwrap().success());
    assert!(oracle.join().unwrap());
    let encoded = serde_json::to_string(&glm).unwrap();
    assert!(!encoded.contains("synthetic-private-key"));
    let catalog_saved = catalog.with_extension("held");
    std::fs::rename(&catalog, &catalog_saved).unwrap();
    assert!(glm.validate().is_err());
    std::fs::rename(&catalog_saved, &catalog).unwrap();
    glm.validate().unwrap();
    let mut changed: Value = serde_json::from_str(&encoded).unwrap();
    changed["version"] = 3.into();
    assert!(serde_json::from_value::<Projection>(changed).is_err());
    let temp = path.with_file_name("changed.tmp");
    write(&temp, br#"{"OPENAI_API_KEY":"changed"}"#);
    std::fs::rename(&temp, &path).unwrap();
    assert!(glm.secret().is_err());
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o775)).unwrap();
    assert!(cutex::launch::selected_profile::review_auth(&path, &Route::GlmApiKey).is_err());
    println!("PASS: two accounts/shared home; native account/read; atomic same-account review; foreign rejection; GLM private child HTTP; no turns");
}
