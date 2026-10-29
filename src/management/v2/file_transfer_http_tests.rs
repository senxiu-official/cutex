//! Real Management dispatcher/socket coverage using the existing access harness pattern.
//! All credentials and source bytes below are synthetic private-instance fixtures.
use crate::config::instance::{self, Selection};
use crate::management::{
    client_tokens::ClientTokenStore,
    listeners::{self, ConnectionInfo},
    server::{handle_management_request, ManagementRequestContext},
    users::{Grant, Projects, UserStore, OWNER},
};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    time::Duration,
};

const ROOT: &str = "transfer-http-test-root-credential";
use crate::management::client_tokens::DeviceScope;

const SAMPLE: &[u8] = b"\0binary\xff transfer\r\n";
const SECRET: &str = "synthetic-transfer-secret-never-return-remotely";
const ZERO_REVISION: &str = "0000000000000000000000000000000000000000000000000000000000000000";

fn query_path(base: &str, path: &str, revision: Option<&str>) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query.append_pair("path", path);
    let suffix = if let Some(revision) = revision {
        query.append_pair("revision", revision);
        query.append_pair("offset", "0");
        query.append_pair("maxBytes", "4194304");
        "/chunk"
    } else {
        ""
    };
    format!("{base}{suffix}?{}", query.finish())
}

fn transfer_context() -> ManagementRequestContext {
    let mut context = p2_context();
    // Only the workspace-root callback is synthetic; authentication, access
    // checks, session resolution, file resolution and HTTP writing are real.
    context.read_management_project = |_, id| {
        let config = crate::config::paths::config_dir().unwrap();
        let root = match id.as_str() {
            "transfer-project" => config.parent().unwrap().join("work"),
            "transfer-secrets-project" => config,
            other => panic!("unexpected project callback: {other}"),
        };
        Ok(serde_json::from_value(json!({
            "project_id": id,
            "authority_epoch": 1,
            "access_role": "human_management",
            "operator_grant_revision": 1,
            "agent_operators": [],
            "presentation": {"display_name":"Transfer", "badge_label":"T", "color":"cyan", "revision":1, "stored":false},
            "active_agents": [], "retired_agents": [],
            "workspace_root": root.to_string_lossy()
        })).unwrap())
    };
    context
}

fn get(path: &str, bearer: Option<&str>) -> (u16, String, Vec<u8>) {
    exchange_with(
        transfer_context(),
        "GET",
        path,
        bearer,
        Vec::new(),
        "application/json",
        "",
    )
}

fn denied(path: &str, bearer: Option<&str>, status: u16, expected: &str) {
    let (actual, _, bytes) = get(path, bearer);
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        (actual, any_code(&body)),
        (status, expected),
        "{path}: {body}"
    );
    assert!(!String::from_utf8_lossy(&bytes).contains(SECRET));
}

fn revision(base: &str, path: &str, bearer: &str) -> String {
    let (status, _, bytes) = get(&query_path(base, path, None), Some(bearer));
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(status, 200, "{base}: {body}");
    assert!(body["path"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(body["name"].is_string());
    assert!(body["size"].is_u64());
    assert!(body["modifiedAt"].is_string());
    let revision = body["revision"].as_str().unwrap();
    assert_eq!(revision.len(), 64);
    assert!(revision.bytes().all(|b| b.is_ascii_hexdigit()));
    revision.into()
}

fn header<'a>(headers: &'a str, name: &str) -> &'a str {
    headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim())
        .unwrap_or_else(|| panic!("missing {name}: {headers}"))
}

fn assert_content(base: &str, path: &str, bearer: &str, expected: &[u8]) {
    let revision = revision(base, path, bearer);
    let (status, headers, bytes) = get(&query_path(base, path, Some(&revision)), Some(bearer));
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    assert_eq!(bytes, expected);
    assert_eq!(header(&headers, "Content-Type"), "application/octet-stream");
    assert_eq!(
        header(&headers, "Content-Length"),
        expected.len().to_string()
    );
    assert_eq!(header(&headers, "ETag"), format!("\"{revision}\""));
    assert_eq!(header(&headers, "X-Cutex-File-Offset"), "0");
    for name in [
        "X-Cutex-File-Bytes",
        "X-Cutex-File-Total-Bytes",
        "X-Cutex-File-Next-Offset",
    ] {
        assert_eq!(header(&headers, name), expected.len().to_string(), "{name}");
    }
    assert_eq!(header(&headers, "X-Cutex-File-Eof"), "true");
}

fn sessions(instance: &PrivateInstance) {
    let work = instance.root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("sample.bin"), SAMPLE).unwrap();
    let mut store = crate::session::model::CutexSessionStore::default();
    for (id, root, durable, exposed) in [
        ("cutex.transfer-visible", work.clone(), true, true),
        ("cutex.transfer-denied", work, true, true),
        ("cutex.transfer-broad", instance.root.clone(), true, true),
        (
            "cutex.transfer-secrets",
            crate::config::paths::config_dir().unwrap(),
            true,
            true,
        ),
        (
            "cutex.transfer-local-hidden",
            instance.root.join("work"),
            false,
            false,
        ),
    ] {
        let mut record = crate::session::model::CutexSessionRecord::new(
            id.into(),
            None,
            crate::platform::host::current_host_name(),
            root.to_string_lossy().into_owned(),
            None,
        )
        .unwrap();
        record.exposed_to_backend = exposed;
        if durable {
            record.registration_class = crate::agent_bus::model::AgentRegistrationClass::Persistent;
        }
        store.sessions.insert(id.into(), record);
    }
    crate::session::store::save_cutex_session_store(&store).unwrap();
}

fn reader(scope: DeviceScope) -> String {
    let tokens = ClientTokenStore::open_default().unwrap();
    let users = UserStore::open_default().unwrap();
    // Complete legacy-token migration before issuing our member device.
    users.snapshot().unwrap();
    let member = users
        .create_user(
            "Transfer reader",
            None,
            Grant {
                projects: Projects::Only(vec!["transfer-project".into()]),
                standalone_agents: vec![
                    "cutex.transfer-visible".into(),
                    "cutex.transfer-broad".into(),
                    "cutex.transfer-local-hidden".into(),
                ],
                ..Grant::default()
            },
            "root",
        )
        .unwrap();
    let token = tokens.issue_scoped("transfer-reader", scope).unwrap();
    users
        .bind_token(&token.token_id, Some(&member.id), "root")
        .unwrap();
    token.token
}

#[test]
fn transfer_http_authenticated_large_binary_roundtrip_and_stale_revision_conflict() {
    use sha2::{Digest, Sha256};

    let _lock = crate::config::paths::test_home_env_lock();
    let instance = PrivateInstance::new();
    let source = instance.root.join("中文 large binary.bin");
    let original: Vec<u8> = (0..11 * 1024 * 1024 + 37)
        .map(|index| (index % 256) as u8)
        .collect();
    std::fs::write(&source, &original).unwrap();
    let users = UserStore::open_default().unwrap();
    users.snapshot().unwrap();
    let tokens = ClientTokenStore::open_default().unwrap();
    let token = tokens
        .issue_scoped("large-transfer-read-device", DeviceScope::Read)
        .unwrap();
    users
        .bind_token(&token.token_id, Some(OWNER), "root")
        .unwrap();

    let path = source.to_str().unwrap();
    let rev = revision("/v2/fs/transfer", path, &token.token);
    let mut accumulated = Vec::with_capacity(original.len());
    let mut hasher = Sha256::new();
    const MAX_BYTES: usize = 4 * 1024 * 1024;
    while accumulated.len() < original.len() {
        let offset = accumulated.len();
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("path", path)
            .append_pair("revision", &rev)
            .append_pair("maxBytes", &MAX_BYTES.to_string())
            .append_pair("offset", &offset.to_string())
            .finish();
        let (status, headers, bytes) = get(
            &format!("/v2/fs/transfer/chunk?{query}"),
            Some(&token.token),
        );
        assert_eq!(status, 200, "chunk at {offset}: {headers}");
        let expected_len = MAX_BYTES.min(original.len() - offset);
        let next = offset + expected_len;
        assert_eq!(bytes.len(), expected_len);
        assert_eq!(header(&headers, "Content-Type"), "application/octet-stream");
        assert_eq!(header(&headers, "Content-Length"), bytes.len().to_string());
        assert_eq!(header(&headers, "ETag"), format!("\"{rev}\""));
        assert_eq!(header(&headers, "X-Cutex-File-Offset"), offset.to_string());
        assert_eq!(
            header(&headers, "X-Cutex-File-Bytes"),
            bytes.len().to_string()
        );
        assert_eq!(
            header(&headers, "X-Cutex-File-Total-Bytes"),
            original.len().to_string()
        );
        assert_eq!(
            header(&headers, "X-Cutex-File-Next-Offset"),
            next.to_string()
        );
        assert_eq!(
            header(&headers, "X-Cutex-File-Eof"),
            if next == original.len() {
                "true"
            } else {
                "false"
            }
        );
        assert_eq!(bytes, original[offset..next], "chunk bytes at {offset}");
        hasher.update(&bytes);
        accumulated.extend_from_slice(&bytes);
    }
    assert_eq!(hasher.finalize(), Sha256::digest(&original));
    assert_eq!(accumulated, original);

    // Different length guarantees an observable revision change even when
    // filesystem timestamps have coarse resolution; no timing sleep is needed.
    std::fs::write(&source, b"changed after complete authenticated transfer").unwrap();
    let (status, _, bytes) = get(
        &query_path("/v2/fs/transfer", path, Some(&rev)),
        Some(&token.token),
    );
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!((status, any_code(&body)), (409, "file_changed"), "{body}");
}

#[test]
fn transfer_http_requires_human_auth_for_metadata_and_chunks_on_every_route() {
    let _lock = crate::config::paths::test_home_env_lock();
    let instance = PrivateInstance::new();
    sessions(&instance);
    let absolute = instance
        .root
        .join("work/sample.bin")
        .to_string_lossy()
        .into_owned();
    for (base, path) in [
        ("/v2/fs/transfer", absolute.as_str()),
        (
            "/v2/sessions/cutex.transfer-visible/fs/transfer",
            "sample.bin",
        ),
        ("/v2/projects/transfer-project/fs/transfer", "sample.bin"),
    ] {
        for revision in [None, Some(ZERO_REVISION)] {
            let path = query_path(base, path, revision);
            for bearer in [None, Some("synthetic-wrong-management-bridge")] {
                denied(&path, bearer, 401, "unauthorized");
            }
        }
    }
}

#[test]
fn transfer_http_even_the_configured_management_bridge_cannot_download_files() {
    let _lock = crate::config::paths::test_home_env_lock();
    let instance = PrivateInstance::new();
    sessions(&instance);
    const BRIDGE: &str = "synthetic-configured-management-bridge";
    let absolute = instance
        .root
        .join("work/sample.bin")
        .to_string_lossy()
        .into_owned();
    for (base, path) in [
        ("/v2/fs/transfer", absolute.as_str()),
        (
            "/v2/sessions/cutex.transfer-visible/fs/transfer",
            "sample.bin",
        ),
        ("/v2/projects/transfer-project/fs/transfer", "sample.bin"),
    ] {
        for revision in [None, Some(ZERO_REVISION)] {
            let path = query_path(base, path, revision);
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {BRIDGE}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
            let client = std::thread::spawn(move || {
                let mut socket = TcpStream::connect(address).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                socket.write_all(request.as_bytes()).unwrap();
                let mut response = Vec::new();
                socket.read_to_end(&mut response).unwrap();
                response
            });
            let (mut server, _) = listener.accept().unwrap();
            listeners::set_current_connection(Some(ConnectionInfo {
                remote: false,
                peer: "127.0.0.1:43210".parse().unwrap(),
            }));
            listeners::set_current_client_token(None);
            crate::management::users::set_current_principal(None);
            crate::http::server::set_keep_alive(crate::http::server::KeepAlive::Off);
            crate::http::server::set_extra_response_headers(String::new());
            crate::http::server::set_accept_gzip(false);
            let handled = handle_management_request(
                &mut server,
                Some(BRIDGE),
                Some(ROOT),
                transfer_context(),
            );
            listeners::set_current_connection(None);
            listeners::set_current_client_token(None);
            crate::management::users::set_current_principal(None);
            drop(server);
            handled.unwrap();
            let response = client.join().unwrap();
            let split = response
                .windows(4)
                .position(|bytes| bytes == b"\r\n\r\n")
                .unwrap();
            assert!(
                response.starts_with(b"HTTP/1.1 401 "),
                "{}",
                String::from_utf8_lossy(&response)
            );
            let body: Value = serde_json::from_slice(&response[split + 4..]).unwrap();
            assert_eq!(any_code(&body), "unauthorized");
        }
    }
}

#[test]
fn transfer_http_member_read_grants_cover_both_scopes_but_never_host_or_hidden_roots() {
    let _lock = crate::config::paths::test_home_env_lock();
    let instance = PrivateInstance::new();
    sessions(&instance);
    for scope in [DeviceScope::Full, DeviceScope::Read, DeviceScope::Chat] {
        let token = reader(scope);
        for base in [
            "/v2/sessions/cutex.transfer-visible/fs/transfer",
            "/v2/projects/transfer-project/fs/transfer",
        ] {
            assert_content(base, "sample.bin", &token, SAMPLE);
        }
        for revision in [None, Some(ZERO_REVISION)] {
            let absolute = instance
                .root
                .join("work/sample.bin")
                .to_string_lossy()
                .into_owned();
            denied(
                &query_path("/v2/fs/transfer", &absolute, revision),
                Some(&token),
                403,
                "owner_required",
            );
            for (base, path, status, code) in [
                (
                    "/v2/sessions/cutex.transfer-denied/fs/transfer",
                    "sample.bin",
                    404,
                    "session_not_found",
                ),
                (
                    "/v2/sessions/cutex.transfer-local-hidden/fs/transfer",
                    "sample.bin",
                    404,
                    "session_not_found",
                ),
                (
                    "/v2/projects/transfer-denied/fs/transfer",
                    "sample.bin",
                    404,
                    "project_not_found",
                ),
                (
                    "/v2/sessions/cutex.transfer-broad/fs/transfer",
                    "work/sample.bin",
                    403,
                    "workspace_root_too_broad",
                ),
            ] {
                denied(
                    &query_path(base, path, revision),
                    Some(&token),
                    status,
                    code,
                );
            }
        }
    }
}

#[test]
fn transfer_http_remote_and_paired_clients_cannot_read_config_or_profile_link_metadata_or_chunks() {
    let _lock = crate::config::paths::test_home_env_lock();
    let instance = PrivateInstance::new();
    sessions(&instance);
    let config = crate::config::paths::config_dir().unwrap();
    std::fs::create_dir_all(config.join("runtime/profile-link")).unwrap();
    let paths = [
        "config.json",
        "runtime/profile-link/secret-v1",
        "runtime/profile-link/.secret-v1.tmp-synthetic",
    ];
    for path in paths {
        let contents = if path == "config.json" {
            json!({"syntheticTransferSecret": SECRET}).to_string()
        } else {
            SECRET.into()
        };
        std::fs::write(config.join(path), contents).unwrap();
    }
    let users = UserStore::open_default().unwrap();
    users.snapshot().unwrap();
    let tokens = ClientTokenStore::open_default().unwrap();
    let cli = tokens.issue("transfer-owner-cli").unwrap();
    let (paired, _) = tokens
        .issue_device("transfer-owner-device", "Synthetic device", "other")
        .unwrap();
    users
        .bind_token(&cli.token_id, Some(OWNER), "root")
        .unwrap();
    users
        .bind_token(&paired.token_id, Some(OWNER), "root")
        .unwrap();
    // Acquire real revisions locally: denial of chunks must not depend on a
    // malformed or stale revision short-circuiting before secret classification.
    for relative in paths {
        let absolute = config.join(relative).to_string_lossy().into_owned();
        for (base, path) in [
            ("/v2/fs/transfer", absolute.as_str()),
            ("/v2/sessions/cutex.transfer-secrets/fs/transfer", relative),
            (
                "/v2/projects/transfer-secrets-project/fs/transfer",
                relative,
            ),
        ] {
            let rev = revision(base, path, ROOT);
            for (remote, bearer) in [
                (false, paired.token.as_str()),
                (true, paired.token.as_str()),
                (true, cli.token.as_str()),
                (true, ROOT),
            ] {
                REMOTE.with(|flag| flag.set(remote));
                // Reset before assertions so a failed assertion cannot taint
                // another test on the same worker thread.
                let results = [None, Some(rev.as_str())]
                    .map(|revision| get(&query_path(base, path, revision), Some(bearer)));
                REMOTE.with(|flag| flag.set(false));
                for (status, _, bytes) in results {
                    let value: Value = serde_json::from_slice(&bytes).unwrap();
                    let expected = if remote && bearer == ROOT {
                        (401, "remote_credential_refused")
                    } else {
                        (403, "owner_local_only")
                    };
                    assert_eq!(
                        (status, any_code(&value)),
                        expected,
                        "{base} {relative}: {value}"
                    );
                    assert!(!String::from_utf8_lossy(&bytes).contains(SECRET));
                }
            }
            let contents = std::fs::read(config.join(relative)).unwrap();
            assert_content(base, path, ROOT, &contents);
            assert_content(base, path, &cli.token, &contents);
        }
    }
}

#[test]
fn transfer_http_workspace_traversal_and_git_internals_are_denied_for_metadata_and_chunks() {
    let _lock = crate::config::paths::test_home_env_lock();
    let instance = PrivateInstance::new();
    sessions(&instance);
    std::fs::write(instance.root.join("outside.bin"), SECRET).unwrap();
    std::fs::create_dir(instance.root.join("work/.git")).unwrap();
    std::fs::write(instance.root.join("work/.git/config"), SECRET).unwrap();
    let token = reader(DeviceScope::Read);
    for base in [
        "/v2/sessions/cutex.transfer-visible/fs/transfer",
        "/v2/projects/transfer-project/fs/transfer",
    ] {
        for revision in [None, Some(ZERO_REVISION)] {
            for (path, code) in [
                ("../outside.bin", "path_outside_root"),
                ("sub/../../outside.bin", "path_outside_root"),
                (".git/config", "path_forbidden"),
            ] {
                denied(&query_path(base, path, revision), Some(&token), 403, code);
            }
        }
    }
}

#[cfg(any(unix, windows))]
#[test]
fn transfer_http_workspace_symlink_escape_is_denied_for_metadata_and_chunks() {
    let _lock = crate::config::paths::test_home_env_lock();
    let instance = PrivateInstance::new();
    sessions(&instance);
    let outside = instance.root.join("outside.bin");
    std::fs::write(&outside, SECRET).unwrap();
    let link = instance.root.join("work/escape.bin");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    #[cfg(windows)]
    if let Err(error) = std::os::windows::fs::symlink_file(&outside, &link) {
        if error.raw_os_error() == Some(1314) {
            eprintln!("SKIP symlink escape: Windows requires Developer Mode or symlink privilege");
            return;
        }
        panic!("create synthetic symlink: {error}");
    }
    let token = reader(DeviceScope::Read);
    let rev = revision("/v2/fs/transfer", outside.to_str().unwrap(), ROOT);
    for base in [
        "/v2/sessions/cutex.transfer-visible/fs/transfer",
        "/v2/projects/transfer-project/fs/transfer",
    ] {
        for revision in [None, Some(rev.as_str())] {
            denied(
                &query_path(base, "escape.bin", revision),
                Some(&token),
                403,
                "path_outside_root",
            );
        }
    }
}

thread_local! {
    static REMOTE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
fn any_code(value: &Value) -> &str {
    value
        .pointer("/error/code")
        .or_else(|| value.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("")
}
struct PrivateInstance {
    root: PathBuf,
    previous: Selection,
}

impl PrivateInstance {
    fn new() -> Self {
        let root = crate::config::paths::home_dir()
            .unwrap()
            .join(format!("access-http-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(crate::config::paths::PRIVATE_HOME_MARKER), b"").unwrap();
        let previous = instance::selected().unwrap();
        instance::set_test_selection(Selection::Custom(root.join(".cutex")));
        // Every fixture starts with an empty registry. Finish its empty
        // journal recovery before exercising routes whose callbacks must not
        // be invoked; otherwise batch order can trigger an unrelated load.
        crate::management::v2::repository::management_v2_repository()
            .unwrap()
            .materialize_recovery_reset(&[], true)
            .unwrap();
        Self { root, previous }
    }
}

impl Drop for PrivateInstance {
    fn drop(&mut self) {
        listeners::set_current_connection(None);
        listeners::set_current_client_token(None);
        crate::management::users::set_current_principal(None);
        instance::set_test_selection(self.previous.clone());
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn context() -> ManagementRequestContext {
    // Denied and gate-answered requests never reach native runtime callbacks.
    ManagementRequestContext {
        explicit_launch_action: |_, _| panic!("unexpected runtime action"),
        adopt_saved_native: |_, _| panic!("unexpected native adoption"),
        review_agent_archive: |_, _| panic!("unexpected archive review"),
        owner_archive_action: |_, _| panic!("unexpected permanent archive"),
        execute_agent_archive: |_, _| panic!("unexpected archive"),
        durable_agent_candidates: |_| panic!("unexpected agent candidates"),
        import_durable_agent: |_, _| panic!("unexpected agent import"),
        load_registry: || panic!("unexpected registry load"),
        load_runtime_status: |_| panic!("unexpected runtime status"),
        forward_native_request: |_, _| panic!("unexpected native request"),
        respond_native_server_request: |_, _, _| panic!("unexpected native response"),
        handle_user_input: |_| panic!("unexpected user input"),
        flush_user_input_queue: |_, _, _| panic!("unexpected queue flush"),
        steer_user_input_queue: |_, _, _| panic!("unexpected queue steer"),
        interrupt_active_turn: |_, _| panic!("unexpected interrupt"),
        load_bootstrap_state: |_, _| panic!("unexpected bootstrap"),
        load_latest_plan: |_| panic!("unexpected plan"),
        load_latest_token_usage: |_| panic!("unexpected usage"),
        revert_session_history: |_, _, _| panic!("unexpected revert"),
        delete_session: |_, _| panic!("unexpected session delete"),
        fork_session: |_, _, _| panic!("unexpected fork"),
        side_chat: |_, _, _| panic!("unexpected side chat"),
        open_offline_history: |_| panic!("unexpected history"),
        mutate_session: |_, _, _| panic!("unexpected session mutation"),
        online_session: |_, _, _| panic!("unexpected online request"),
        bind_project_authority: |_| panic!("unexpected project bind"),
        import_legacy_director_ownership: |_| panic!("unexpected ownership import"),
        reconcile_agent_reservation: |_| panic!("unexpected reservation"),
        abandon_agent_create: |_| panic!("unexpected reservation"),
        list_management_projects: |_| panic!("unexpected project list"),
        read_management_project: |_, _| panic!("unexpected project read"),
        update_management_project_presentation: |_, _| panic!("unexpected project update"),
        execute_management_operator_action: |_, _| panic!("unexpected operator action"),
        execute_management_project_mutation: |_, _| panic!("unexpected project mutation"),
        create_human_agent: |_, _| panic!("unexpected agent create"),
        create_human_session: |_, _| panic!("unexpected session create"),
    }
}

fn exchange_with(
    context: ManagementRequestContext,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: Vec<u8>,
    content_type: &str,
    extra: &str,
) -> (u16, String, Vec<u8>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n",
        body.len()
    ).into_bytes();
    if let Some(bearer) = bearer {
        request.extend_from_slice(format!("Authorization: Bearer {bearer}\r\n").as_bytes());
    }
    request.extend_from_slice(extra.as_bytes());
    request.extend_from_slice(b"\r\n");
    request.extend_from_slice(&body);
    let client = std::thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream.write_all(&request).unwrap();
        let mut response = vec![];
        stream.read_to_end(&mut response).unwrap();
        response
    });
    let (mut server, _) = listener.accept().unwrap();
    listeners::set_current_connection(Some(ConnectionInfo {
        remote: REMOTE.with(std::cell::Cell::get),
        peer: "127.0.0.1:43210".parse().unwrap(),
    }));
    listeners::set_current_client_token(None);
    crate::management::users::set_current_principal(None);
    crate::http::server::set_keep_alive(crate::http::server::KeepAlive::Off);
    crate::http::server::set_extra_response_headers(String::new());
    crate::http::server::set_accept_gzip(false);
    let handled = handle_management_request(&mut server, None, Some(ROOT), context);
    listeners::set_current_connection(None);
    listeners::set_current_client_token(None);
    crate::management::users::set_current_principal(None);
    drop(server);
    handled.unwrap();
    let response = client.join().unwrap();
    let split = response.windows(4).position(|v| v == b"\r\n\r\n").unwrap();
    let headers = std::str::from_utf8(&response[..split]).unwrap();
    let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, headers.to_string(), response[split + 4..].to_vec())
}

fn p2_context() -> ManagementRequestContext {
    let mut context = context();
    context.load_registry = || Ok(Default::default());
    context.load_runtime_status = |_| Ok(None);
    context.handle_user_input = |_| {
        Err(crate::management::v2::user_input::UserInputExecutionError {
            stage: "route".into(),
            code: "test_user_input_reached".into(),
            message: "the access gate let the input through".into(),
            retryable: false,
            details: json!({}),
            outcome_unknown: false,
        })
    };
    context
}
