use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use cutex::agent_bus::model::AgentRegistrationClass;
use cutex::session::model::{CutexSessionRecord, CutexSessionStore};
use cutex::session::store::save_cutex_session_store;

const FIXTURE_SESSION_ID: &str = "cutex.archive-fixture";
/// Marker that makes a directory an accepted `CUTEX_TEST_PRIVATE_HOME`
/// (`src/config/paths.rs`); debug `cutex` children refuse unmarked homes.
const PRIVATE_HOME_MARKER: &str = ".cutex-test-private-home";

struct DisposableHome(PathBuf);

impl DisposableHome {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "cutex-session-archive-cli-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("disposable home should be created");
        fs::write(path.join(PRIVATE_HOME_MARKER), b"")
            .expect("private test home marker should be written");
        Self(path)
    }
}

impl Drop for DisposableHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn archive_command(home: &Path, command: &str, id: &str, json: bool) -> Command {
    let mut process = Command::new(env!("CARGO_BIN_EXE_cutex"));
    process
        .args(["session", command, id])
        .env("CUTEX_TEST_PRIVATE_HOME", home)
        .env("HOME", home)
        .env_remove("CUTEX_HOME")
        .env_remove("CUTEX_MANAGEMENT_URL")
        .env_remove("CUTEX_AGENT_ID")
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env_remove("OPENAI_API_KEY")
        .env_remove("CODEX_HOME")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if json {
        process.arg("--json");
    }
    process
}

/// A loopback port in the Management range that nothing listens on (never the
/// live 24270 / 24260 or the dev 24370 / 24360 daemons).
fn closed_management_port() -> u16 {
    (24800..=24999)
        .find_map(|port| std::net::TcpListener::bind(("127.0.0.1", port)).ok())
        .map(|listener| listener.local_addr().expect("listener address").port())
        .expect("a free loopback port for the closed Management fixture")
}

/// What the guarded Management path legitimately needs in this home: the
/// private root credential (fresh per fixture) and a port of its own. No
/// service is started, so the port answers nothing.
fn provision_management_credential(home: &Path) -> u16 {
    let port = closed_management_port();
    with_fixture_home(home, || {
        let mut config = cutex::config::store::load_codez_config();
        config.management_api_token = Some(cutex::profiles::model::ManagementApiToken::new(
            format!("archive-cli-fixture-{}", uuid::Uuid::new_v4().simple()),
        ));
        config.management_port = Some(port);
        cutex::config::store::save_codez_config(&config).expect("fixture config should save");
    });
    port
}

fn invoke_missing_archive_command(command: &str, json: bool) -> Output {
    let home = DisposableHome::new();
    provision_management_credential(&home.0);
    archive_command(&home.0, command, "missing-session", json)
        .output()
        .expect("archive command should execute")
}

fn json_error(output: Output) -> serde_json::Value {
    assert!(!output.status.success(), "archive command should fail");
    assert!(output.stdout.is_empty(), "JSON failure must not use stdout");
    assert!(
        !output.stderr.contains(&0x1b),
        "JSON failure must not include ANSI: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    let error: serde_json::Value = serde_json::from_str(stderr.trim()).expect("stderr JSON object");
    assert!(error.is_object(), "stderr must contain one JSON object");
    error
}

fn assert_route_lookup_error(error: serde_json::Value) {
    assert_eq!(error["stage"], "route");
    assert_eq!(error["code"], "session_not_found");
    assert_eq!(
        error["message"],
        "cutex session is not known: missing-session"
    );
    assert_eq!(error["retryable"], false);
    assert_eq!(error["details"]["cutexSessionId"], "missing-session");
    assert_eq!(error["outcomeUnknown"], false);
}

#[test]
fn retire_and_restore_json_lookup_failures_are_typed_process_envelopes() {
    assert_route_lookup_error(json_error(invoke_missing_archive_command("retire", true)));
    assert_route_lookup_error(json_error(invoke_missing_archive_command("restore", true)));
}

#[test]
fn retire_human_lookup_failure_keeps_human_presentation() {
    let output = invoke_missing_archive_command("retire", false);

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(stderr.contains("error:"));
    assert!(stderr.contains("cutex session is not known: missing-session"));
}

#[test]
fn known_agent_without_management_service_is_a_guard_failure() {
    let home = DisposableHome::new();
    let port = provision_management_credential(&home.0);
    let mut record = CutexSessionRecord::new_at(
        FIXTURE_SESSION_ID.to_string(),
        Some("019e-archive-fixture".to_string()),
        "fixture-host".to_string(),
        "/tmp/cutex-archive-fixture".to_string(),
        Some("fixture".to_string()),
        "2026-08-14T00:00:00Z".to_string(),
    )
    .expect("fixture record should be valid");
    record.registration_class = AgentRegistrationClass::Persistent;
    with_fixture_home(&home.0, || {
        let mut store = CutexSessionStore::default();
        store
            .sessions
            .insert(record.cutex_session_id.clone(), record);
        save_cutex_session_store(&store).expect("fixture store should save");
    });

    let error = json_error(
        archive_command(&home.0, "retire", FIXTURE_SESSION_ID, true)
            .output()
            .expect("archive command should execute"),
    );
    assert_eq!(error["stage"], "route");
    assert_eq!(error["code"], "guarded_archive_failed");
    assert_eq!(error["details"]["cutexSessionId"], FIXTURE_SESSION_ID);
    assert_eq!(error["outcomeUnknown"], false);
    let message = error["message"].as_str().expect("message");
    assert!(
        message.contains(&format!("unavailable on port {port}")),
        "{message}"
    );
}

static FIXTURE_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

struct HomeEnvironment {
    previous_home: Option<OsString>,
}

impl HomeEnvironment {
    fn set(home: &Path) -> Self {
        let previous_home = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", home) };
        Self { previous_home }
    }
}

impl Drop for HomeEnvironment {
    fn drop(&mut self) {
        match &self.previous_home {
            Some(home) => unsafe { std::env::set_var("HOME", home) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }
}

fn with_fixture_home<T>(home: &Path, operation: impl FnOnce() -> T) -> T {
    let _lock = FIXTURE_ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("fixture environment lock should not be poisoned");
    let _environment = HomeEnvironment::set(home);
    operation()
}
