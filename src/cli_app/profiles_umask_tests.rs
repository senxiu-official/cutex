//! API profile creation, human-agent preflight and durable adoption under a
//! permissive umask. Native protocol/history are synthetic; no model is called.
//! The daemon home is initialized privately; the profiles tree starts absent.

use cutex::http::server::SimpleHttpRequest;
use serde_json::{json, Value};
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

const CHILD: &str = "CUTEX_PROFILES_UMASK_CHILD";

fn root_request(method: &str, path: &str, body: Value) -> (u16, Value) {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let client = std::thread::spawn(move || {
        let mut socket = std::net::TcpStream::connect(address).unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).unwrap();
        response
    });
    let (mut socket, _) = listener.accept().unwrap();
    let request = SimpleHttpRequest {
        method: method.into(),
        path: path.into(),
        headers: [("authorization".into(), "Bearer root-test".into())]
            .into_iter()
            .collect(),
        body: serde_json::to_vec(&body).unwrap(),
    };
    cutex::management::v2::server::handle_v2_request(
        &mut socket,
        &request,
        Some("bridge-test"),
        Some("root-test"),
        super::management_context::management_request_context(),
    )
    .unwrap();
    drop(socket);
    let response = client.join().unwrap();
    let status = response[9..12].parse().unwrap();
    let body = serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
    (status, body)
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn api_profile_umask_002_creates_human_agent() {
    // The launch auth review trusts only ancestors owned by root or the current user. In a user
    // namespace sandbox (bwrap --unshare-all) `/` shows up as nobody, so the review can never pass.
    {
        use std::os::unix::fs::MetadataExt;
        let root = std::fs::metadata("/").unwrap().uid();
        if root != 0 && root != unsafe { libc::geteuid() } {
            eprintln!("skipped: `/` is owned by uid {root} (user namespace sandbox)");
            return;
        }
    }
    if std::env::var_os(CHILD).is_none() {
        let parent = std::path::PathBuf::from(
            std::env::var_os("CUTEX_TEST_PRIVATE_HOME").expect("private test home required"),
        );
        assert!(parent.join(".cutex-test-private-home").is_file());
        let home = parent.join(format!(
            "umask-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&home)
            .unwrap();
        std::fs::write(
            home.join(".cutex-test-private-home"),
            "profiles umask regression",
        )
        .unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "cli_app::profiles_umask_tests::api_profile_umask_002_creates_human_agent",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("HOME", &home)
            .env("TMPDIR", &home)
            .env("CUTEX_TEST_PRIVATE_HOME", &home)
            .env_remove("CUTEX_HOME")
            .env_remove("CODEX_HOME");
        // umask is process-global, so change it only in the isolated child.
        unsafe {
            command.pre_exec(|| {
                libc::umask(0o002);
                Ok(())
            });
        }
        let output = command.output().unwrap();
        std::fs::remove_dir_all(&home).unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let home = cutex::config::paths::home_dir().unwrap();
    let probe = home.join("umask-probe");
    std::fs::create_dir(&probe).unwrap();
    assert_eq!(mode(&probe), 0o775, "fixture must actually use umask 002");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(home.join(".cutex"))
        .unwrap();
    assert!(!home.join(".cutex/profiles").exists());

    let (status, body) = root_request(
        "POST",
        "/v2/profiles",
        json!({
            "name": "umask-profile", "kind": "apiKey",
            "baseUrl": "http://127.0.0.1:9/v1", "model": "test-model", "apiKey": "synthetic-test-key"
        }),
    );
    assert_eq!(status, 201, "{body}");
    let id = body["profile"]["id"].as_str().unwrap();
    let profiles = home.join(".cutex/profiles");
    assert_eq!(mode(&profiles), 0o700);
    assert_eq!(mode(&profiles.join(id)), 0o700);

    let (status, duplicate) = root_request(
        "POST",
        &format!("/v2/profiles/{id}/duplicate"),
        json!({"name": "umask-copy"}),
    );
    assert_eq!(status, 201, "{duplicate}");
    assert_eq!(
        mode(&profiles.join(duplicate["profile"]["id"].as_str().unwrap())),
        0o700
    );
    assert_eq!(duplicate["credentialsCopied"], false);

    let native_home = cutex::config::paths::host_codex_home_dir().unwrap();
    std::fs::create_dir(&native_home).unwrap();
    std::fs::write(
        native_home.join("config.toml"),
        "model_reasoning_effort = 'medium'\n",
    )
    .unwrap();

    install_synthetic_native(&home, &native_home);
    let configuration = super::light_new::human_configuration(
        Some(id),
        cutex::config::new_thread::ThreadKind::Agent,
        None,
    )
    .expect("API-created profile must pass the real agent configuration and auth review");
    assert!(configuration.selected_projection.is_some());

    let request = |request_id: &str| {
        json!({
            "requestId": request_id, "name": "umask-agent", "profile": id, "cwd": home,
        })
    };
    let (status, body) = root_request("POST", "/v2/human/agents", request("umask-native"));
    assert!(status == 200 || status == 201, "{status}: {body}");
    let session = body["cutexSessionId"].as_str().unwrap();
    let store = cutex::session::store::load_cutex_session_store().unwrap();
    let record = &store.sessions[session];
    assert!(record.agent_enabled && record.exposed_to_backend);
    assert_eq!(record.profile.as_deref(), Some("umask-profile"));
    assert_eq!(record.formal_agent_name.as_deref(), Some("umask-agent"));
    assert!(record.explicit_launch.is_some());

    // Old homes are not silently chmod'ed. The same API must still report the
    // existing unsafe ancestor, rather than weakening its launch validation.
    std::fs::set_permissions(&profiles, std::fs::Permissions::from_mode(0o775)).unwrap();
    let (status, body) = root_request("POST", "/v2/human/agents", request("umask-existing-unsafe"));
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["code"], "unsupported_profile");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("auth ancestor is group/other writable"),
        "{body}"
    );
    assert_eq!(mode(&profiles), 0o775);
}

fn install_synthetic_native(home: &Path, native_home: &Path) {
    let binary = home.join("synthetic-native");
    std::fs::write(&binary, r#"#!/usr/bin/python3
import base64, hashlib, json, os, pathlib, socket, struct, sys
native_id = "12345678-1234-4234-8234-123456789abc"
endpoint = sys.argv[sys.argv.index("--listen") + 1].removeprefix("unix://")
server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
server.bind(endpoint)
server.listen(1)
connection, _ = server.accept()
reader = connection.makefile("rb")
assert reader.readline().startswith(b"GET ")
headers = {}
while True:
    line = reader.readline()
    if line == b"\r\n":
        break
    name, value = line.decode().split(":", 1)
    headers[name.lower()] = value.strip()
accept = base64.b64encode(hashlib.sha1((headers["sec-websocket-key"] + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
connection.sendall(("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: " + accept + "\r\n\r\n").encode())
def send_frame(payload, opcode=1):
    length = len(payload)
    header = bytes([128 | opcode])
    if length < 126:
        header += bytes([length])
    elif length <= 65535:
        header += bytes([126]) + struct.pack("!H", length)
    else:
        header += bytes([127]) + struct.pack("!Q", length)
    connection.sendall(header + payload)
while True:
    prefix = reader.read(2)
    if not prefix:
        break
    opcode, length = prefix[0] & 15, prefix[1] & 127
    if length == 126:
        length = struct.unpack("!H", reader.read(2))[0]
    elif length == 127:
        length = struct.unpack("!Q", reader.read(8))[0]
    mask = reader.read(4) if prefix[1] & 128 else None
    payload = reader.read(length)
    if mask:
        payload = bytes(value ^ mask[index % 4] for index, value in enumerate(payload))
    if opcode == 8:
        break
    if opcode == 9:
        send_frame(payload, 10)
        continue
    assert opcode == 1 and prefix[0] & 128
    request = json.loads(payload)
    if "id" not in request:
        continue
    method = request["method"]
    if method == "initialize":
        result = {"userAgent": "synthetic/0.160.0", "externalInputVersion": 1}
    elif method in ("thread/start", "thread/read"):
        if method == "thread/start":
            sessions = pathlib.Path(os.environ["CODEX_HOME"]) / "sessions"
            sessions.mkdir(exist_ok=True)
            history = sessions / ("rollout-2026-10-12T00-00-00-" + native_id + ".jsonl")
            history.write_text(json.dumps({"type": "session_meta", "payload": {"id": native_id, "cwd": os.getcwd()}}) + "\n")
        result = {"thread": {"id": native_id, "cwd": os.getcwd(), "historyMode": "paginated", "turns": []}}
    else:
        raise AssertionError("unexpected native method: " + method)
    send_frame(json.dumps({"id": request["id"], "result": result}).encode())
"#).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let companion = home.join("codex-code-mode-host");
    let facade = home.join("synthetic-facade");
    let schema = home.join("synthetic-schema.json");
    for path in [&companion, &facade, &schema] {
        std::fs::write(path, "{}").unwrap();
    }
    let verified = |path: &Path| {
        json!({
            "path": path, "sha256": cutex::agent_management::file_sha256(path).unwrap(),
        })
    };
    let manifest = home.join("bundle.json");
    std::fs::write(
        &manifest,
        serde_json::to_vec(&json!({
            "version": 4, "upstream_commit": "synthetic-test-only",
            "executable": verified(&binary), "code_mode_host": verified(&companion),
            "facade": verified(&facade), "schema": verified(&schema),
            "shared_config": verified(&native_home.join("config.toml")),
        }))
        .unwrap(),
    )
    .unwrap();
    cutex::launch::local_deployment::LocalDeployment {
        native_home: native_home.into(),
        bundle_manifest: manifest,
        job_mcp: None,
    }
    .install()
    .unwrap();
}
