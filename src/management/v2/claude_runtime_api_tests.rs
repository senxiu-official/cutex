use super::*;
use crate::management::v2::schema_export::assert_valid;
use crate::profiles::lifecycle::tests::TestHome;
use crate::runtime::claude_pin::PREVIOUS_FILE;
use std::path::{Path, PathBuf};

fn install_runtime(root: &Path, name: &str, home: &Path) -> ClaudeDeployment {
    let rt = root.join(name);
    let adapter = rt.join("adapter");
    std::fs::create_dir_all(adapter.join("src/cute_claude")).unwrap();
    std::fs::create_dir_all(adapter.join("scripts")).unwrap();
    for file in ["src/cute_claude/__main__.py", "scripts/connect_tui.py"] {
        std::fs::write(adapter.join(file), "").unwrap();
    }
    let python = rt.join("python");
    std::fs::write(&python, "").unwrap();
    ClaudeDeployment {
        python,
        adapter_source: adapter,
        claude_cli: root.join("claude"),
        claude_home: home.to_path_buf(),
        tui_binary: root.join("tui"),
    }
}

struct Fixture {
    _home: TestHome,
    root: PathBuf,
    r17: ClaudeDeployment,
    r19: ClaudeDeployment,
}

fn fixture() -> Fixture {
    let home = TestHome::new();
    let root = PathBuf::from(std::env::var_os("HOME").unwrap());
    for file in ["claude", "tui"] {
        std::fs::write(root.join(file), "").unwrap();
    }
    let login = root.join("shared-login");
    std::fs::create_dir(&login).unwrap();
    let r17 = install_runtime(&root, "claude-r17", &login);
    let r19 = install_runtime(&root, "claude-r19", &login);
    r17.install().unwrap();
    Fixture { _home: home, root, r17, r19 }
}

fn agent(f: &Fixture, name: &str) -> CutexSessionRecord {
    crate::runtime::claude::create(name, &f.root, "sonnet", false, false).unwrap()
}

fn record(id: &str) -> CutexSessionRecord {
    crate::session::store::load_cutex_session_store().unwrap().sessions[id].clone()
}

fn set_runtime_pid(id: &str, pid: Option<u32>) {
    let mut store = crate::session::store::load_cutex_session_store().unwrap();
    store.sessions.get_mut(id).unwrap().runtime_pid = pid;
    crate::session::store::save_cutex_session_store(&store).unwrap();
}

fn pin(id: &str) -> ClaudeDeployment {
    record(id).claude_session.unwrap().deployment
}

#[test]
fn paths() {
    assert_eq!(parse_path("/v2/sessions/cutex.s1/claude-runtime"), Some(Route::View("cutex.s1".into())));
    assert_eq!(parse_path("/v2/sessions/cutex.s1/claude-runtime?x=1"), Some(Route::View("cutex.s1".into())));
    assert_eq!(parse_path("/v2/sessions/cutex.s1/claude-runtime/repin"), Some(Route::Repin("cutex.s1".into())));
    assert_eq!(parse_path("/v2/sessions//claude-runtime"), None);
    assert_eq!(parse_path("/v2/sessions/a/b/claude-runtime"), None);
    assert_eq!(parse_path("/v2/sessions/a%2Fb/claude-runtime/repin"), None);
    assert!(!is_path("/v2/sessions/cutex.s1/claude-code"));
    assert!(!is_path("/v2/sessions/cutex.s1/claude-runtime/other"));
    assert!(!super::super::claude_code_sources::is_path("/v2/sessions/cutex.s1/claude-runtime"));
}

#[test]
fn direction_compares_release_numbers() {
    assert_eq!(direction("claude-r9", "claude-r10", true), "upgrade");
    assert_eq!(direction("claude-r19", "claude-r17", true), "downgrade");
    assert_eq!(direction("claude-r17", "claude-r17", true), "change");
    assert_eq!(direction("claude-r17", "claude-r19", false), "none");
}

#[test]
fn offline_agent_upgrades_now_and_then_reports_nothing_to_do() {
    let f = fixture();
    let id = agent(&f, "offline-upgrade").cutex_session_id;

    // Selected = the agent's runtime: nothing to offer.
    let same = view(&record(&id), &Ok(f.r17.clone()));
    assert_valid("claude-runtime", "runtimeView", &same);
    assert_eq!((same["direction"].as_str(), same["upgradeAvailable"].as_bool()), (Some("none"), Some(false)));
    assert_eq!(same["dryRun"]["outcome"], "unchanged");

    let shown = view(&record(&id), &Ok(f.r19.clone()));
    assert_valid("claude-runtime", "runtimeView", &shown);
    assert_eq!(shown["current"]["release"], "claude-r17");
    assert_eq!(shown["selected"]["release"], "claude-r19");
    assert_eq!(shown["direction"], "upgrade");
    assert_eq!(shown["upgradeAvailable"], true);
    assert_eq!(shown["dryRun"], json!({"outcome": "applied", "reason": null, "restartRequired": false}));
    assert_eq!(pin(&id), f.r17, "the view changes nothing");

    let done = repin_now(&id, &RepinRequest { target: Some("claude-r19".into()) }, Ok(f.r19.clone())).unwrap();
    assert_valid("claude-runtime", "repinResponse", &done);
    assert_eq!(done["result"]["outcome"], "applied");
    assert_eq!((done["result"]["previous"].as_str(), done["result"]["target"].as_str()), (Some("claude-r17"), Some("claude-r19")));
    assert_eq!(done["runtime"]["current"]["release"], "claude-r19");
    assert_eq!(done["runtime"]["upgradeAvailable"], false);
    assert_eq!(pin(&id), f.r19);
    assert!(record(&id).claude_session.unwrap().state_dir.join(PREVIOUS_FILE).is_file());

    // Repeating it is a no-op; an older selected runtime is a downgrade, not offered.
    assert_eq!(repin_now(&id, &RepinRequest::default(), Ok(f.r19.clone())).unwrap()["result"]["outcome"], "unchanged");
    let older = view(&record(&id), &Ok(f.r17.clone()));
    assert_eq!((older["direction"].as_str(), older["upgradeAvailable"].as_bool()), (Some("downgrade"), Some(false)));
}

#[test]
fn online_agent_defers_to_its_next_start_without_restarting() {
    let f = fixture();
    let id = agent(&f, "online-upgrade").cutex_session_id;
    set_runtime_pid(&id, Some(std::process::id()));

    let shown = view(&record(&id), &Ok(f.r19.clone()));
    assert_eq!(shown["dryRun"]["outcome"], "deferred");
    assert_eq!(shown["dryRun"]["restartRequired"], true);
    assert_eq!(shown["upgradeAvailable"], true);

    let done = repin_now(&id, &RepinRequest::default(), Ok(f.r19.clone())).unwrap();
    assert_valid("claude-runtime", "repinResponse", &done);
    assert_eq!(done["result"]["outcome"], "deferred");
    assert_eq!(done["result"]["restartRequired"], true);
    assert_eq!(pin(&id), f.r17, "a running agent keeps its pin");
    assert_eq!(record(&id).runtime_pid, Some(std::process::id()), "and is not restarted");
    assert_eq!(done["runtime"]["pending"]["release"], "claude-r19");
    assert_eq!(done["runtime"]["upgradeAvailable"], false, "already pending");

    // The next fresh start applies it.
    set_runtime_pid(&id, None);
    let started = crate::runtime::claude_pin::apply_pending_at_start(record(&id)).unwrap();
    assert_eq!(started.claude_session.unwrap().deployment, f.r19);
    assert_eq!(view(&record(&id), &Ok(f.r19.clone()))["pending"], Value::Null);
}

#[test]
fn refusals_change_nothing() {
    let f = fixture();
    let created = agent(&f, "legacy-state");
    let id = created.cutex_session_id.clone();

    // A changed selection since the client looked.
    let changed = repin_now(&id, &RepinRequest { target: Some("claude-r18".into()) }, Ok(f.r19.clone())).unwrap_err();
    assert_eq!((changed.status, changed.code), (409, "claude_runtime_changed"));
    assert_eq!(changed.details["selected"], "claude-r19");

    // No usable selected runtime.
    let missing = repin_now(&id, &RepinRequest::default(), Err(anyhow::anyhow!("not installed"))).unwrap_err();
    assert_eq!(missing.code, "claude_runtime_not_installed");
    let without = view(&record(&id), &Err(anyhow::anyhow!("not installed")));
    assert_valid("claude-runtime", "runtimeView", &without);
    assert_eq!((without["selected"].clone(), without["dryRun"].clone()), (Value::Null, Value::Null));

    // A schema-2 adapter state cannot move to a target that is not r8+.
    let state_dir = created.claude_session.unwrap().state_dir;
    std::fs::create_dir_all(&state_dir).unwrap();
    let db = rusqlite::Connection::open(state_dir.join("state.sqlite3")).unwrap();
    db.execute_batch("CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL); INSERT INTO meta VALUES('schema','2');").unwrap();
    let shown = view(&record(&id), &Ok(f.r19.clone()));
    assert_valid("claude-runtime", "runtimeView", &shown);
    assert_eq!(shown["upgradeAvailable"], true, "offered, with the refusal in dryRun");
    assert_eq!(shown["dryRun"]["outcome"], "skipped");
    assert!(shown["dryRun"]["reason"].as_str().unwrap().contains("restore the pre-r8"));
    let refused = repin_now(&id, &RepinRequest::default(), Ok(f.r19.clone())).unwrap_err();
    assert_eq!((refused.status, refused.code), (409, "claude_repin_refused"));
    assert!(refused.message.contains("restore the pre-r8"));
    assert_eq!(refused.details["target"], "claude-r19");
    assert_eq!(pin(&id), f.r17);
    assert!(!state_dir.join(PENDING_FILE).exists());

    // Not a Claude agent / unknown session.
    assert_eq!(claude_record("cutex.missing").unwrap_err().code, "session_not_found");
    let mut store = crate::session::store::load_cutex_session_store().unwrap();
    let codex = CutexSessionRecord::new(
        "cutex.codex-agent".into(), None, crate::platform::host::current_host_name(),
        f.root.to_string_lossy().into_owned(), None,
    ).unwrap();
    store.sessions.insert(codex.cutex_session_id.clone(), codex);
    crate::session::store::save_cutex_session_store(&store).unwrap();
    assert_eq!(claude_record("cutex.codex-agent").unwrap_err().code, "unsupported_runtime");
}

#[test]
fn request_body_is_strict() {
    assert!(serde_json::from_value::<RepinRequest>(json!({})).is_ok());
    assert!(serde_json::from_value::<RepinRequest>(json!({"target": "claude-r19"})).is_ok());
    assert!(serde_json::from_value::<RepinRequest>(json!({"force": true})).is_err());
    assert_valid("claude-runtime", "repinRequest", &json!({"target": "claude-r19"}));
}

/// session-claude-runtime: the resource carries the pin and the pending
/// repin, the list the selected release, so the desktop's menu needs no
/// per-agent request; the watcher sees each change.
#[test]
fn session_resources_carry_the_pin_and_the_list_the_selected_release() {
    use crate::management::v2::session::{project_session_entry, retired_session_resource_value, SessionProjectionEntry};
    use crate::management::v2::sessions_watch::claude_stamps;
    let f = fixture();
    let id = agent(&f, "pin-in-resource").cutex_session_id;
    let project = |id: &str| {
        project_session_entry(&record(id), true, None, None, &json!({}), &mut SessionProjectionEntry::default()).unwrap()
    };
    let store = || crate::session::store::load_cutex_session_store().unwrap();

    let resource = project(&id);
    assert_valid("sessions", "session", &resource);
    assert_eq!(resource["runtime"]["claudeRuntime"], json!({"release": "claude-r17", "pendingRelease": null}));
    assert_eq!(host_selected_json(), json!({"selectedRelease": "claude-r17"}));
    let list = json!({"contractVersion": 2, "hostId": "h", "claudeRuntime": host_selected_json(), "sessions": [resource]});
    assert_valid("sessions", "sessionList", &list);
    let before = claude_stamps(&store());

    // `cutex human claude install` of a newer runtime: the list offers it.
    f.r19.install().unwrap();
    assert_eq!(host_selected_json(), json!({"selectedRelease": "claude-r19"}));
    let installed = claude_stamps(&store());
    assert_ne!(installed.selected, before.selected, "the watcher sees the selection change");
    assert_eq!(installed.pending, before.pending);

    // A running agent: the repin waits for its next start (pendingRelease).
    set_runtime_pid(&id, Some(std::process::id()));
    let revision = |value: &Value| value["runtime"]["claudeRuntime"].clone();
    assert_eq!(repin_now(&id, &RepinRequest::default(), Ok(f.r19.clone())).unwrap()["result"]["outcome"], "deferred");
    let deferred = project(&id);
    assert_valid("sessions", "session", &deferred);
    assert_eq!(revision(&deferred), json!({"release": "claude-r17", "pendingRelease": "claude-r19"}));
    assert_ne!(claude_stamps(&store()).pending[&id], installed.pending[&id], "the watcher sees the pending repin");

    // The next fresh start applies it.
    set_runtime_pid(&id, None);
    crate::runtime::claude_pin::apply_pending_at_start(record(&id)).unwrap();
    assert_eq!(revision(&project(&id)), json!({"release": "claude-r19", "pendingRelease": null}));

    // Archived Claude agents carry it too; other sessions do not.
    let mut retired = record(&id);
    retired.archive_state = crate::session::model::CutexSessionArchiveState::Retired;
    assert_eq!(revision(&retired_session_resource_value(&retired, None))["release"], "claude-r19");
    let codex = CutexSessionRecord::new(
        "cutex.codex-pin".into(), None, crate::platform::host::current_host_name(),
        f.root.to_string_lossy().into_owned(), None,
    ).unwrap();
    assert_eq!(session_pin_json(&codex), None);
    let codex_resource = project_session_entry(&codex, true, None, None, &json!({}), &mut SessionProjectionEntry::default()).unwrap();
    assert!(codex_resource["runtime"].get("claudeRuntime").is_none());

    // No usable runtime selected: null.
    std::fs::remove_file(crate::runtime::claude::deployment_path().unwrap()).unwrap();
    assert_eq!(host_selected_json(), json!({"selectedRelease": null}));
    assert_valid("sessions", "sessionList", &json!({"contractVersion": 2, "hostId": "h", "claudeRuntime": host_selected_json(), "sessions": []}));
    assert!(crate::management::v2::capabilities::capabilities_resource()["featureList"]
        .as_array().unwrap().iter().any(|f| f == SESSION_FIELD_CAPABILITY));
}
