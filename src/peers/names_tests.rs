use super::{names, pair, store::Store, tests::peer};
use crate::agent_names::{NameCandidate, NameClaim, NameResolutionRequest, NameScope, RenameNote};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::TcpListener,
    time::Duration,
};

struct PrivateDirectory(std::path::PathBuf);
impl PrivateDirectory {
    fn new() -> Self {
        let p = crate::config::paths::home_dir()
            .unwrap()
            .join(format!("peer-name-unit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&p).unwrap();
        Self(p)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for PrivateDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const A: &str = "cutex.00000000-0000-0000-0000-0000000000a1";
const B: &str = "cutex.00000000-0000-0000-0000-0000000000b2";

fn query(target: &str, scope: Option<NameScope>) -> NameResolutionRequest {
    NameResolutionRequest {
        target: target.into(),
        scope,
    }
}
fn project(id: &str) -> NameScope {
    NameScope::Project {
        project_id: id.into(),
    }
}
fn candidate(id: &str, name: &str, scope: NameScope) -> NameCandidate {
    NameCandidate {
        cutex_session_id: id.into(),
        current_name: name.into(),
        scope,
        archived: false,
        retired: false,
    }
}

/// Real bounded HTTP exchanges (not a cache lookup or a substituted resolver).
fn authority(
    features: Vec<String>,
    response: Option<Value>,
) -> (
    Store,
    super::store::Peer,
    std::thread::JoinHandle<()>,
    PrivateDirectory,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut p = peer();
    p.urls = vec![format!("http://{}", listener.local_addr().unwrap())];
    let id = p.peer_id.clone();
    let instance = super::instance_id();
    let dir = PrivateDirectory::new();
    let store = Store::at(dir.path().join("peers"));
    let secret = super::token::generate();
    store.insert(p.clone(), &secret).unwrap();
    store.settings(&json!({"enabled":true})).unwrap();
    let thread = std::thread::spawn(move || {
        let hello = json!({"instanceId":id,"protocol":1,"features":features});
        let mut answers = vec![("GET /peer/v1/hello HTTP/1.1".to_string(), hello)];
        if let Some(response) = response {
            answers.push((format!("POST {} HTTP/1.1", names::ROUTE), response));
        }
        for (expected, answer) in answers {
            let until = std::time::Instant::now() + Duration::from_secs(10);
            let mut socket = loop {
                match listener.accept() {
                    Ok((s, _)) => break s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < until,
                            "missing authority request {expected}"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut raw = Vec::new();
            let mut chunk = [0; 4096];
            loop {
                let n = socket.read(&mut chunk).unwrap();
                assert!(n > 0);
                raw.extend_from_slice(&chunk[..n]);
                if let Some(at) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&raw[..at]);
                    let len = head
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|s| s.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if raw.len() >= at + 4 + len {
                        break;
                    }
                }
                assert!(raw.len() < 65536);
            }
            let raw = String::from_utf8(raw).unwrap();
            assert!(raw.starts_with(&expected));
            assert!(raw.contains(&secret));
            assert!(raw.contains(&instance));
            if expected.starts_with("POST") {
                let request: NameResolutionRequest =
                    serde_json::from_str(raw.split_once("\r\n\r\n").unwrap().1).unwrap();
                assert_eq!(request.target, "old");
                assert_eq!(request.scope, Some(project("red")));
            }
            let body = serde_json::to_vec(&answer).unwrap();
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            socket.write_all(&body).unwrap();
        }
    });
    (store, p, thread, dir)
}

#[test]
fn successful_stale_cache_hit_never_binds_the_cached_recipient() {
    let _lock = crate::config::paths::test_home_env_lock();
    let visible = [candidate(A, "new", project("red"))];
    let claims = [NameClaim {
        scope: project("red"),
        normalized_name: "old".into(),
        cutex_session_id: A.into(),
    }];
    let reply = names::reply(crate::agent_names::resolve(
        "old",
        Some(&project("red")),
        &visible,
        &claims,
        19,
    ));
    let (store, p, thread, _dir) =
        authority(pair::features(), Some(serde_json::to_value(reply).unwrap()));
    std::fs::write(
        store.dir.join(format!("directory-{}.json", p.peer_id)),
        json!({"agents":[{"cutexSessionId":B,"name":"old"}]}).to_string(),
    )
    .unwrap();
    let (_, resolved) = names::resolve(&store, &p, &query("old", Some(project("red")))).unwrap();
    assert_eq!(resolved["cutexSessionId"], A);
    assert_eq!(resolved["indexRevision"], 19);
    assert_eq!(
        names::detail(&resolved),
        "old was renamed by the owner to new; use new from now on"
    );
    thread.join().unwrap();
}

#[test]
fn downgrade_refuses_names_even_when_cached_and_previously_negotiated() {
    let _lock = crate::config::paths::test_home_env_lock();
    let (store, mut p, thread, _dir) =
        authority(pair::FEATURES.iter().map(|s| s.to_string()).collect(), None);
    p.negotiated_features.push(names::FEATURE.into());
    std::fs::write(
        store.dir.join(format!("directory-{}.json", p.peer_id)),
        json!({"agents":[{"cutexSessionId":B,"name":"old"}]}).to_string(),
    )
    .unwrap();
    let error = names::resolve(&store, &p, &query("old", None)).unwrap_err();
    assert_eq!(error.code, "peer_name_resolution_unsupported");
    assert!(error.details["detail"]
        .as_str()
        .unwrap()
        .contains("full cutex.<UUID>@"));
    assert!(!store
        .get(&p.peer_id)
        .unwrap()
        .negotiated_features
        .iter()
        .any(|f| f == names::FEATURE));
    thread.join().unwrap();
}

#[test]
fn old_peer_accepts_full_id_without_resolving_a_cache_or_short_alias() {
    let _lock = crate::config::paths::test_home_env_lock();
    let (store, p, thread, _dir) =
        authority(pair::FEATURES.iter().map(|s| s.to_string()).collect(), None);
    let (_, target) = names::resolve(&store, &p, &query(A, Some(project("red")))).unwrap();
    assert_eq!(target["cutexSessionId"], A);
    thread.join().unwrap();
    assert!(!names::full_session_id("a1"));
    assert!(!names::full_session_id("cutex.a1"));
}

#[test]
fn scoped_and_host_wide_ambiguity_stays_structured_over_the_wire() {
    let visible = [
        candidate(A, "new", project("red")),
        candidate(B, "old", project("blue")),
    ];
    let claims = [NameClaim {
        scope: project("red"),
        normalized_name: "old".into(),
        cutex_session_id: A.into(),
    }];
    let result = crate::agent_names::resolve("old", None, &visible, &claims, 11);
    let value = serde_json::to_value(names::reply(result)).unwrap();
    assert_eq!(value["outcome"], "refused");
    assert_eq!(value["failure"]["kind"], "ambiguous");
    assert_eq!(value["failure"]["candidates"].as_array().unwrap().len(), 2);
    let _: names::Reply = serde_json::from_value(value).unwrap();
    assert_eq!(
        crate::agent_names::resolve("old", Some(&project("red")), &visible, &claims, 11)
            .unwrap()
            .cutex_session_id,
        A
    );
}

#[test]
fn archive_and_retirement_refusals_roundtrip_without_old_receipt_extensions() {
    for retired in [false, true] {
        let mut target = candidate(A, "old", NameScope::Unassigned);
        target.archived = !retired;
        target.retired = retired;
        let value = serde_json::to_value(names::reply(crate::agent_names::resolve(
            "old",
            None,
            &[target],
            &[],
            1,
        )))
        .unwrap();
        assert_eq!(
            value["failure"]["kind"],
            if retired { "retired" } else { "archived" }
        );
        let _: names::Reply = serde_json::from_value(value).unwrap();
    }
    let note = RenameNote {
        requested_name: "old".into(),
        current_name: "new".into(),
        cutex_session_id: A.into(),
        scope: NameScope::Unassigned,
    };
    assert!(note.text().contains("owner"));
}

#[test]
fn request_and_reply_are_strict_and_director_roles_stay_local() {
    assert!(serde_json::from_value::<NameResolutionRequest>(
        json!({"target":"old","scope":{"kind":"project","projectId":"red"},"guess":true})
    )
    .is_err());
    assert!(names::validate(&query("director", None)).is_err());
    assert!(names::validate(&query("old\n", None)).is_err());
    assert!(names::validate(&query("old", Some(project("red")))).is_ok());
    assert!(serde_json::from_value::<names::Reply>(
        json!({"outcome":"refused","failure":{"kind":"notFound"},"extra":true})
    )
    .is_err());
}

#[test]
fn retry_binding_survives_rename_reuse_and_does_not_evict_at_many_rows() {
    let _lock = crate::config::paths::test_home_env_lock();
    let dir = PrivateDirectory::new();
    let store = Store::at(dir.path().join("peers"));
    let p = peer();
    let key = super::name_bindings::Key {
        caller: A,
        tool: "agent_tell",
        action: "lost-response",
        digest: "original-request",
    };
    assert!(super::name_bindings::lookup(&store, &key)
        .unwrap()
        .is_none());
    let original = json!({"cutexSessionId":A,"name":"old"});
    super::name_bindings::bind(&store, &key, &p, &original).unwrap();
    let changed = json!({"cutexSessionId":B,"name":"old"});
    assert_eq!(
        super::name_bindings::bind(&store, &key, &p, &changed)
            .unwrap()
            .1,
        original
    );
    for n in 0..64 {
        let action = format!("other-{n}");
        let other = super::name_bindings::Key {
            caller: A,
            tool: "agent_tell",
            action: &action,
            digest: "other-request",
        };
        super::name_bindings::bind(&store, &other, &p, &changed).unwrap();
    }
    assert_eq!(
        super::name_bindings::lookup(&store, &key)
            .unwrap()
            .unwrap()
            .1,
        original
    );
    let conflict = super::name_bindings::Key {
        digest: "changed-request",
        ..key
    };
    assert_eq!(
        super::name_bindings::lookup(&store, &conflict)
            .unwrap_err()
            .code,
        "idempotency_conflict"
    );
}

#[test]
fn concurrent_binding_returns_the_same_first_durable_target() {
    let _lock = crate::config::paths::test_home_env_lock();
    let dir = PrivateDirectory::new();
    let store = Store::at(dir.path().join("peers"));
    let p = peer();
    let setup = super::name_bindings::Key {
        caller: A,
        tool: "agent_tell",
        action: "setup",
        digest: "setup",
    };
    super::name_bindings::bind(&store, &setup, &p, &json!({"cutexSessionId":A})).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = [A, B]
        .into_iter()
        .map(|id| {
            let directory = store.dir.clone();
            let p = p.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let store = Store::at(directory);
                barrier.wait();
                let key = super::name_bindings::Key {
                    caller: A,
                    tool: "agent_tell",
                    action: "race",
                    digest: "same-request",
                };
                super::name_bindings::bind(&store, &key, &p, &json!({"cutexSessionId":id})).unwrap()
            })
        })
        .collect();
    let selected: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(selected[0], selected[1]);
}

#[test]
fn generic_local_and_remote_pins_share_one_store_without_rebinding() {
    use crate::name_target_pins::{self as pins, Binding, PinError};
    let _lock = crate::config::paths::test_home_env_lock();
    let dir = PrivateDirectory::new();
    let store = Store::at(dir.path().join("peers"));
    let key = pins::Key {
        caller: A,
        tool: "agent_tell",
        action: "local-scope",
        digest: "original-raw-digest",
    };
    let local = Binding {
        peer_id: None,
        cutex_session_id: A.into(),
        detail: json!({"scope":project("red"),"currentName":"old"}),
    };
    assert_eq!(pins::bind_at(dir.path(), &key, &local).unwrap(), local);
    let changed = Binding {
        cutex_session_id: B.into(),
        detail: json!({"scope":project("blue"),"currentName":"old"}),
        ..local.clone()
    };
    assert_eq!(pins::bind_at(dir.path(), &key, &changed).unwrap(), local);
    assert_eq!(
        super::name_bindings::lookup(&store, &key).unwrap_err().code,
        "idempotency_conflict"
    );
    let other = pins::Key {
        action: "remote-scope",
        ..key
    };
    let p = peer();
    let remote = json!({"cutexSessionId":B,"name":"old"});
    super::name_bindings::bind(&store, &other, &p, &remote).unwrap();
    let from_generic = pins::lookup_at(dir.path(), &other).unwrap().unwrap();
    assert_eq!(from_generic.peer_id, Some(p.peer_id));
    assert_eq!(from_generic.cutex_session_id, B);
    assert!(dir.path().join("name-target-pins.sqlite3").is_file());
    let different = pins::Key {
        digest: "different-raw-digest",
        ..key
    };
    assert_eq!(
        pins::lookup_at(dir.path(), &different).unwrap_err(),
        PinError::Conflict
    );
}

/// Fixture writer for the two-real-daemon acceptance driver. It is ignored in
/// normal suites, writes only a marked private HOME, never starts a runtime,
/// and uses real session/roster persistence rather than a substitute resolver.
#[test]
#[ignore]
fn private_peer_name_fixture() {
    let _lock = crate::config::paths::test_home_env_lock();
    let root = crate::config::paths::home_dir().unwrap();
    assert!(root
        .join(crate::config::paths::PRIVATE_HOME_MARKER)
        .is_file());
    let input = std::path::PathBuf::from(
        std::env::var_os("CUTEX_PEER_NAME_FIXTURE").expect("fixture path"),
    );
    assert!(input
        .canonicalize()
        .unwrap()
        .starts_with(root.canonicalize().unwrap()));
    let fixture: Value = serde_json::from_slice(&std::fs::read(input).unwrap()).unwrap();
    let home = root.join(".cutex");
    let previous = crate::config::instance::selected().unwrap();
    crate::config::instance::set_test_selection(crate::config::instance::Selection::Custom(home));
    let result = std::panic::catch_unwind(|| {
        if fixture["mode"] == "seed" {
            let mut sessions = crate::session::model::CutexSessionStore::default();
            for row in fixture["agents"].as_array().unwrap() {
                let id = row["id"].as_str().unwrap();
                assert!(names::full_session_id(id));
                let mut record = crate::session::model::CutexSessionRecord::new(
                    id.into(),
                    Some(id.strip_prefix("cutex.").unwrap().into()),
                    "private-peer-fixture".into(),
                    root.to_string_lossy().into(),
                    None,
                )
                .unwrap();
                record.agent_enabled = true;
                record.formal_agent_name = Some(row["name"].as_str().unwrap().into());
                sessions.sessions.insert(id.into(), record);
            }
            crate::session::store::save_cutex_session_store(&sessions).unwrap();
            let roster = crate::agent_management::AgentManagementStore::open_default().unwrap();
            roster.with_state(true,|mut state| {
                for row in fixture["agents"].as_array().unwrap() {
                    let id=row["id"].as_str().unwrap();
                    let managed:crate::agent_management::ManagedAgentRecord=serde_json::from_value(json!({
                        "project_id":row.get("project"),"created_by_director_session":null,"cutex_session_id":id,
                        "native_session_id":id.strip_prefix("cutex.").unwrap(),
                        "spec":{"name":row["name"],"cwd":root.to_string_lossy(),"profile":"synthetic-profile",
                            "runtime_backend":"codex","model":"synthetic","reasoning":"low","permissions":"full",
                            "approval_policy":"never","sandbox_mode":"danger-full-access","groups":["default"]},
                        "created_at":"2026-11-05T00:00:00Z"
                    })).unwrap();
                    state.agents.insert(crate::role_revision::CutexSessionId::new(id).unwrap(),managed);
                    if let Some(project)=row["project"].as_str() {
                        let authority:crate::agent_management::ProjectAuthority=serde_json::from_value(json!({
                            "project_id":project,"authorized_director_session":row["id"],"authority_epoch":1,"updated_at":"2026-11-05T00:00:00Z"
                        })).unwrap();
                        state.projects.insert(crate::agent_management::ProjectId::new(project).unwrap(),authority);
                    }
                }
                Ok((state,(),true))
            }).unwrap();
        } else if fixture["mode"] == "archive" {
            let path = crate::session::store::cutex_sessions_path().unwrap();
            crate::session::store::update_session_row(
                &path,
                fixture["id"].as_str().unwrap(),
                |record| {
                    record.archive_state = crate::session::model::CutexSessionArchiveState::Retired;
                    Ok(())
                },
            )
            .unwrap()
            .unwrap();
        } else if fixture["mode"] == "retire" {
            let id = fixture["id"].as_str().unwrap();
            let path = crate::session::store::cutex_sessions_path().unwrap();
            let (_, record) = crate::session::store::load_session_row(&path, id)
                .unwrap()
                .unwrap();
            let managed:crate::agent_management::ManagedAgentRecord=serde_json::from_value(json!({
                "project_id":null,"created_by_director_session":null,"cutex_session_id":id,
                "native_session_id":id.strip_prefix("cutex.").unwrap(),
                "spec":{"name":record.formal_agent_name,"cwd":root.to_string_lossy(),"profile":"synthetic-profile",
                    "runtime_backend":"codex","model":"synthetic","reasoning":"low","permissions":"full",
                    "approval_policy":"never","sandbox_mode":"danger-full-access","groups":["default"]},
                "created_at":"2026-11-05T00:00:00Z","retired_at":"2026-11-05T01:00:00Z"
            })).unwrap();
            crate::agent_management::AgentManagementStore::open_default()
                .unwrap()
                .with_state(true, |mut state| {
                    state.agents.insert(
                        crate::role_revision::CutexSessionId::new(id).unwrap(),
                        managed,
                    );
                    Ok((state, (), true))
                })
                .unwrap();
        } else if fixture["mode"] == "resolve" {
            let scope = fixture
                .get("scope")
                .map(|v| serde_json::from_value::<NameScope>(v.clone()).unwrap());
            let output =
                match super::asks::resolve_scoped(fixture["target"].as_str().unwrap(), scope) {
                    Ok((peer, target)) => json!({"ok":true,"peerId":peer.peer_id,"target":target}),
                    Err(error) => error.value(),
                };
            std::fs::write(
                root.join("fixture-result.json"),
                serde_json::to_vec_pretty(&output).unwrap(),
            )
            .unwrap();
        } else if fixture["mode"] == "tell" || fixture["mode"] == "prepare_delegate" {
            let store = crate::asks::store::AskStore::open_default().unwrap();
            let world = crate::asks::scheduler::HostWorld::load();
            let send = |_: &crate::asks::tools::TellSend<'_>| {
                panic!("remote fixture must never use local delivery")
            };
            let chain = |_: &str, _: &str, _: &str| None;
            let ctx = crate::asks::tools::ToolContext {
                store: &store,
                world: &world,
                caller: fixture["caller"].as_str().unwrap(),
                now: chrono::Utc::now(),
                send: &send,
                wake_chain: &chain,
                clock: &chrono::Utc::now,
                sleep: &|| (),
                wait: false,
                facade: crate::asks::tools::FACADE_DELEGATIONS,
            };
            let prepared = if fixture["mode"] == "prepare_delegate" {
                Some(super::asks::prepare_delegate_target(
                    &ctx, fixture["args"]["to"].as_str().unwrap(),
                    fixture["args"]["text"].as_str().unwrap(), None,
                    crate::asks::model::AskOptions::default(),
                    fixture["actionId"].as_str().unwrap(),
                    fixture["args"].get("targetScope").map(|v|serde_json::from_value(v.clone()).unwrap()),
                ).map(|_|json!({"ok":true})))
            } else { None };
            let output = match prepared.unwrap_or_else(||super::tells::send(
                &ctx,
                &fixture["args"],
                fixture["actionId"].as_str().unwrap(),
            )) {
                Ok(value) => value,
                Err(error) => {
                    json!({"ok":false,"code":error.code,"detail":error.detail,"extra":error.extra})
                }
            };
            std::fs::write(
                root.join("fixture-result.json"),
                serde_json::to_vec_pretty(&output).unwrap(),
            )
            .unwrap();
        } else if fixture["mode"] == "message" {
            let repository =
                crate::management::v2::agent_bus_state::agent_bus_message_repository().unwrap();
            let id = fixture["messageId"].as_str().unwrap();
            let message = repository.canonical_message(id).unwrap();
            let snapshot = repository.snapshot_by_message_id(id).unwrap();
            std::fs::write(
                root.join("fixture-result.json"),
                serde_json::to_vec_pretty(&json!({"message":message,"snapshot":snapshot})).unwrap(),
            )
            .unwrap();
        } else {
            panic!("unsupported private fixture mode");
        }
    });
    crate::config::instance::set_test_selection(previous);
    result.unwrap();
}
