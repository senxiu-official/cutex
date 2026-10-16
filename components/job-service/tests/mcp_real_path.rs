#![cfg(target_os = "linux")]

use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const API: &[u8] = b"api-token-for-js2-tests-32-bytes-minimum";
const GRANT: &[u8] = b"grant-key-for-js2-tests-32-bytes-minimum";
const BUS_TOKEN: &str = "isolated-agent-bus-token";
const RUNTIME_ID: &str = "runtime-js2-probe";
const DURABLE_ID: &str = "cutex.js2-real-path";

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_cutex-job-service")
}

fn launcher() -> &'static str {
    static LAUNCHER: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    LAUNCHER.get_or_init(test_launcher)
}

#[test]
fn installed_codex_configured_mcp_binds_real_full_access_origin() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let socket = temp.path().join("run/job.sock");
    let api_file = temp.path().join("api.key");
    let grant_file = temp.path().join("grant.key");
    write_secret(&api_file, API);
    write_secret(&grant_file, GRANT);

    let mut daemon = Command::new(binary())
        .args([
            "serve",
            state.to_str().unwrap(),
            socket.to_str().unwrap(),
            api_file.to_str().unwrap(),
            grant_file.to_str().unwrap(),
            "/bin/false",
            "--allow-launcher",
            launcher(),
        ])
        .spawn()
        .unwrap();
    wait_for_path(&socket, Duration::from_secs(120));

    let observed_thread = Arc::new(Mutex::new(None));
    let (bus_url, bus_stop, bus_worker) = fake_agent_bus(Arc::clone(&observed_thread));
    let child_cwd = temp.path().join("worker worktree 私有");
    std::fs::create_dir(&child_cwd).unwrap();
    let (model_url, model_stop, model_worker) =
        fake_responses_server(&child_cwd, Arc::clone(&observed_thread));
    let codex_home = temp.path().join("codex-home");
    std::fs::create_dir(&codex_home).unwrap();
    let config = format!(
        r#"model = "gpt-5.4"
model_provider = "js2-mock"

[model_providers.js2-mock]
name = "js2-mock"
base_url = {model_url}
wire_api = "responses"
requires_openai_auth = false
supports_websockets = false

[mcp_servers.job_service]
command = {binary}
args = ["mcp-stdio", {socket}, {api}, {grant}, {launcher}]
env_vars = ["CUTEX_AGENT_ID", "CUTEX_AGENT_BUS_URL", "CUTEX_AGENT_BUS_TOKEN"]
startup_timeout_sec = 120
default_tools_approval_mode = "approve"

[code_mode]
direct_only_tool_namespaces = ["mcp__job_service"]
"#,
        model_url = serde_json::to_string(&format!("{model_url}/v1")).unwrap(),
        binary = serde_json::to_string(binary()).unwrap(),
        socket = serde_json::to_string(socket.to_str().unwrap()).unwrap(),
        api = serde_json::to_string(api_file.to_str().unwrap()).unwrap(),
        grant = serde_json::to_string(grant_file.to_str().unwrap()).unwrap(),
        launcher = serde_json::to_string(launcher()).unwrap(),
    );
    std::fs::write(codex_home.join("config.toml"), config).unwrap();

    let mcp_list = Command::new(launcher())
        .args(["mcp", "list"])
        .env("CODEX_HOME", &codex_home)
        .output()
        .unwrap();
    assert!(
        mcp_list.status.success()
            && String::from_utf8_lossy(&mcp_list.stdout).contains("job_service"),
        "configured MCP server is absent: stdout={} stderr={}",
        String::from_utf8_lossy(&mcp_list.stdout),
        String::from_utf8_lossy(&mcp_list.stderr)
    );

    let output = Command::new(launcher())
        .args([
            "exec",
            "--skip-git-repo-check",
            "--ephemeral",
            "--json",
            "--sandbox",
            "danger-full-access",
            "submit the requested isolated test job",
        ])
        .current_dir(temp.path())
        .env("CODEX_HOME", &codex_home)
        .env("CUTEX_AGENT_ID", RUNTIME_ID)
        .env("CUTEX_AGENT_BUS_URL", &bus_url)
        .env("CUTEX_AGENT_BUS_TOKEN", BUS_TOKEN)
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "installed Codex failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    bus_stop.send(()).unwrap();
    model_stop.send(()).unwrap();
    let bus_requests = bus_worker.join().unwrap();
    let model_requests = model_worker.join().unwrap();
    assert_eq!(
        bus_requests.len(),
        1,
        "expected one authenticated Agent Bus lookup; model requests={:?}; codex stdout={} stderr={}",
        model_requests
            .iter()
            .map(|request| summarize_request(request))
            .collect::<Vec<_>>(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(model_requests.len(), 3, "unexpected Responses API requests");

    let store: Value =
        serde_json::from_slice(&std::fs::read(state.join("state.json")).unwrap()).unwrap();
    let jobs = store["jobs"].as_object().unwrap();
    assert_eq!(jobs.len(), 1);
    let job = jobs.values().next().unwrap();
    assert_eq!(job["request"]["subscriberCutexSessionId"], DURABLE_ID);
    assert_eq!(job["request"]["cwd"], child_cwd.to_str().unwrap());
    assert_eq!(job["request"]["origin"]["runtimeAgentId"], RUNTIME_ID);
    assert_eq!(
        job["request"]["origin"]["nativeThreadId"],
        observed_thread.lock().unwrap().clone().unwrap()
    );
    assert_eq!(
        job["request"]["origin"]["permissionProfileType"],
        "disabled"
    );
    let until = Instant::now() + Duration::from_secs(5);
    while !job_output_contains(&state, jobs.keys().next().unwrap(), b"real-path")
        && Instant::now() < until
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(job_output_contains(
        &state,
        jobs.keys().next().unwrap(),
        b"real-path"
    ));

    assert!(job_output_contains(
        &state,
        jobs.keys().next().unwrap(),
        child_cwd.to_str().unwrap().as_bytes()
    ));
    let _ = daemon.kill();
    let _ = daemon.wait();
}

fn fake_agent_bus(
    observed_thread: Arc<Mutex<Option<String>>>,
) -> (String, Sender<()>, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut requests = Vec::new();
        loop {
            if stop_rx.try_recv().is_ok() {
                return requests;
            }
            let (mut stream, _) = match listener.accept() {
                Ok(value) => value,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(error) => panic!("Agent Bus accept failed: {error}"),
            };
            let request = read_http_request(&mut stream);
            assert!(request.starts_with("GET /api/agents?agent_id="));
            assert!(request.contains("&all_hosts=false HTTP/1.1"));
            assert!(request.contains(&format!("Authorization: Bearer {BUS_TOKEN}\r\n")));
            let thread_id = observed_thread
                .lock()
                .unwrap()
                .clone()
                .expect("Responses request must establish the originating thread first");
            let body = serde_json::to_vec(&json!([{
                "id":RUNTIME_ID,
                "session_id":thread_id,
                "cutex_session_id":DURABLE_ID
            }]))
            .unwrap();
            write_http(&mut stream, "application/json", &body);
            requests.push(request);
        }
    });
    (format!("http://{address}"), stop_tx, worker)
}

fn fake_responses_server(
    cwd: &std::path::Path,
    observed_thread: Arc<Mutex<Option<String>>>,
) -> (String, Sender<()>, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let cwd = cwd.to_str().unwrap().to_string();
    let (stop_tx, stop_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut requests = Vec::new();
        loop {
            if stop_rx.try_recv().is_ok() {
                return requests;
            }
            let (mut stream, _) = match listener.accept() {
                Ok(value) => value,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(error) => panic!("Responses API accept failed: {error}"),
            };
            let request = read_http_request(&mut stream);
            assert!(request.starts_with("POST /v1/responses HTTP/1.1"));
            let thread_id = request
                .lines()
                .find_map(|line| line.strip_prefix("thread-id: "))
                .expect("Codex request must carry a thread-id")
                .to_string();
            *observed_thread.lock().unwrap() = Some(thread_id);
            let index = requests.len();
            // The production app-server is long lived. This isolated fresh process must
            // finish hashing the pinned launcher before deferred MCP discovery.
            if index == 0 {
                std::thread::sleep(Duration::from_secs(40));
            }
            let events = if index == 0 {
                vec![
                    json!({"type":"response.created","response":{"id":"resp-js2-search"}}),
                    json!({
                        "type":"response.output_item.done",
                        "item":{
                            "type":"tool_search_call",
                            "call_id":"call-js2-search",
                            "execution":"client",
                            "arguments":json!({
                                "query":"Job Service submit asynchronous command",
                                "limit":4
                            })
                        }
                    }),
                    completed("resp-js2-search"),
                ]
            } else if index == 1 {
                vec![
                    json!({"type":"response.created","response":{"id":"resp-js2-1"}}),
                    json!({
                        "type":"response.output_item.done",
                        "item":{
                            "type":"function_call",
                            "call_id":"call-js2-submit",
                            "namespace":"mcp__job_service",
                            "name":"submit",
                            "arguments":serde_json::to_string(&json!({
                                "actionId":"js2-real-path-action",
                                "argv":["/bin/sh","-c","pwd; printf real-path"],
                                "cwd":cwd
                            })).unwrap()
                        }
                    }),
                    completed("resp-js2-1"),
                ]
            } else {
                vec![
                    json!({"type":"response.output_item.done","item":{"type":"message","role":"assistant","id":"msg-js2","content":[{"type":"output_text","text":"submitted"}]}}),
                    completed("resp-js2-2"),
                ]
            };
            let mut body = String::new();
            for event in events {
                let kind = event["type"].as_str().unwrap();
                body.push_str(&format!("event: {kind}\ndata: {event}\n\n"));
            }
            write_http(&mut stream, "text/event-stream", body.as_bytes());
            requests.push(request);
        }
    });
    (format!("http://{address}"), stop_tx, worker)
}

fn completed(id: &str) -> Value {
    json!({"type":"response.completed","response":{"id":id,"usage":{"input_tokens":0,"input_tokens_details":null,"output_tokens":0,"output_tokens_details":null,"total_tokens":0}}})
}

fn summarize_request(request: &str) -> Value {
    let body = request.split_once("\r\n\r\n").unwrap().1;
    let value: Value = serde_json::from_str(body).unwrap();
    json!({
        "tools": value["tools"].as_array().unwrap().iter().filter_map(|tool| {
            let name = tool.get("name").and_then(Value::as_str)?;
            (name.contains("job") || name == "tool_search").then_some(name)
        }).collect::<Vec<_>>(),
        "inputTail": value["input"].as_array().unwrap().iter().rev().take(2).cloned().collect::<Vec<_>>()
    })
}

fn read_http_request(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buffer[..count]);
        let Some(split) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&bytes[..split]);
        let length = head
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .and_then(|value| value.parse::<usize>().ok())
            })
            .unwrap_or(0);
        if bytes.len() >= split + 4 + length {
            return String::from_utf8(bytes).unwrap();
        }
    }
}

fn write_http(stream: &mut TcpStream, content_type: &str, body: &[u8]) {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .unwrap();
    stream.write_all(body).unwrap();
}

fn write_secret(path: &std::path::Path, value: &[u8]) {
    std::fs::write(path, value).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn wait_for_path(path: &std::path::Path, timeout: Duration) {
    let until = Instant::now() + timeout;
    while !path.exists() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(path.exists(), "path did not appear: {}", path.display());
}

fn job_output_contains(state: &std::path::Path, job_id: &str, expected: &[u8]) -> bool {
    std::fs::read(state.join("output").join(format!("{job_id}.stdout"))).is_ok_and(|bytes| {
        bytes
            .windows(expected.len())
            .any(|window| window == expected)
    })
}

#[test]
fn installed_codex_remote_registration_uses_trusted_identity_and_is_idempotent() {
    let private_home =
        std::env::var_os("CUTEX_TEST_PRIVATE_HOME").expect("private test HOME required");
    assert_eq!(std::env::var_os("HOME"), Some(private_home.clone()));
    assert!(
        std::path::Path::new(&private_home)
            .join(".cutex-test-private-home")
            .is_file()
    );
    for enabled in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let socket = temp.path().join("run/job.sock");
        let api_file = temp.path().join("api.key");
        let grant_file = temp.path().join("grant.key");
        write_secret(&api_file, API);
        write_secret(&grant_file, GRANT);
        let mut command = Command::new(binary());
        command.args([
            "serve",
            state.to_str().unwrap(),
            socket.to_str().unwrap(),
            api_file.to_str().unwrap(),
            grant_file.to_str().unwrap(),
            "/bin/false",
            "--allow-launcher",
            launcher(),
        ]);
        if enabled {
            command.args(["--external-source", "nci"]);
        }
        let daemon = ProbeProcess(command.spawn().unwrap(), false);
        wait_for_path(&socket, Duration::from_secs(120));
        let observed_thread = Arc::new(Mutex::new(None));
        let (bus_url, bus_stop, bus_worker) = fake_agent_bus(Arc::clone(&observed_thread));
        let (model_url, model_stop, model_worker) =
            fake_remote_responses_server(enabled, Arc::clone(&observed_thread));
        let codex_home = temp.path().join("codex-home");
        std::fs::create_dir(&codex_home).unwrap();
        let config = format!(
            r#"model = "gpt-5.4"
model_provider = "remote-mock"
[model_providers.remote-mock]
name = "remote-mock"
base_url = {model_url}
wire_api = "responses"
requires_openai_auth = false
supports_websockets = false
[mcp_servers.job_service]
command = {binary}
args = ["mcp-stdio", {socket}, {api}, {grant}, {launcher}]
env_vars = ["CUTEX_AGENT_ID", "CUTEX_AGENT_BUS_URL", "CUTEX_AGENT_BUS_TOKEN"]
startup_timeout_sec = 120
default_tools_approval_mode = "approve"
[code_mode]
direct_only_tool_namespaces = ["mcp__job_service"]
"#,
            model_url = serde_json::to_string(&format!("{model_url}/v1")).unwrap(),
            binary = serde_json::to_string(binary()).unwrap(),
            socket = serde_json::to_string(socket.to_str().unwrap()).unwrap(),
            api = serde_json::to_string(api_file.to_str().unwrap()).unwrap(),
            grant = serde_json::to_string(grant_file.to_str().unwrap()).unwrap(),
            launcher = serde_json::to_string(launcher()).unwrap(),
        );
        std::fs::write(codex_home.join("config.toml"), config).unwrap();
        let mut native = ProbeProcess(
            Command::new(launcher())
                .args([
                    "exec",
                    "--skip-git-repo-check",
                    "--ephemeral",
                    "--json",
                    "--sandbox",
                    "danger-full-access",
                    "register the isolated external test job",
                ])
                .current_dir(temp.path())
                .env("CODEX_HOME", &codex_home)
                .env("CUTEX_AGENT_ID", RUNTIME_ID)
                .env("CUTEX_AGENT_BUS_URL", &bus_url)
                .env("CUTEX_AGENT_BUS_TOKEN", BUS_TOKEN)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()
                .unwrap(),
            true,
        );
        let deadline = Instant::now() + Duration::from_secs(150);
        let status = loop {
            if let Some(status) = native.0.try_wait().unwrap() {
                break Some(status);
            }
            if Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        drop(native);
        let _ = bus_stop.send(());
        let _ = model_stop.send(());
        let bus_requests = bus_worker.join().unwrap();
        let model_requests = model_worker.join().unwrap();
        drop(daemon);
        assert!(
            status.is_some_and(|status| status.success()),
            "installed Codex remote probe failed"
        );
        assert_eq!(model_requests, if enabled { 5 } else { 2 });
        assert_eq!(bus_requests.len(), if enabled { 3 } else { 0 });
        if !enabled {
            assert!(
                !state.join("state.json").exists(),
                "discovery must not mutate the job store"
            );
            continue;
        }
        let store: Value =
            serde_json::from_slice(&std::fs::read(state.join("state.json")).unwrap()).unwrap();
        let jobs = store["jobs"].as_object().unwrap();
        assert_eq!(jobs.len(), 1, "retry must not register another job");
        let job = jobs.values().next().unwrap();
        assert_eq!(job["request"]["subscriberCutexSessionId"], DURABLE_ID);
        assert_eq!(job["external"]["source"], "nci");
        assert_eq!(job["external"]["remoteId"], "123456.gadi-pbs");
        assert_eq!(job["state"], "registered");
        assert!(job["exitCode"].is_null());
        assert!(job["terminalReason"].is_null());
        assert!(job["processId"].is_null());
        assert!(store["outbox"].as_object().unwrap().is_empty());
    }
}

fn find_remote_receipt(value: &Value) -> Option<Value> {
    match value {
        Value::Object(fields) => {
            if value["correlationToken"].as_str().is_some()
                && value["suggestedJobName"].as_str().is_some()
            {
                return Some(value.clone());
            }
            fields.values().find_map(find_remote_receipt)
        }
        Value::Array(items) => items.iter().find_map(find_remote_receipt),
        Value::String(text) => json_fragments(text).find_map(|value| find_remote_receipt(&value)),
        _ => None,
    }
}

fn fake_remote_responses_server(
    enabled: bool,
    observed_thread: Arc<Mutex<Option<String>>>,
) -> (String, Sender<()>, JoinHandle<usize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut count = 0;
        let mut registration = None;
        loop {
            if stop_rx.try_recv().is_ok() {
                return count;
            }
            let (mut stream, _) = match listener.accept() {
                Ok(value) => value,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(error) => panic!("Responses API accept failed: {error}"),
            };
            let request = read_http_request(&mut stream);
            assert!(request.starts_with("POST /v1/responses HTTP/1.1"));
            *observed_thread.lock().unwrap() = Some(
                request
                    .lines()
                    .find_map(|line| line.strip_prefix("thread-id: "))
                    .unwrap()
                    .to_string(),
            );
            let body: Value =
                serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
            let item = if count == 0 {
                // Allow this isolated fresh process to hash the pinned native launcher.
                std::thread::sleep(Duration::from_secs(40));
                json!({"type":"tool_search_call","call_id":"remote-search","execution":"client",
                    "arguments":{"query":"job_service prepare_remote register_remote external NCI job", "limit":20}})
            } else if count == 1 {
                let discovery = &body;
                assert_eq!(
                    has_tool_name(discovery, "prepare_remote"),
                    enabled,
                    "prepare_remote discovery must follow source configuration"
                );
                assert_eq!(
                    has_tool_name(discovery, "register_remote"),
                    enabled,
                    "register_remote discovery must follow source configuration"
                );
                if enabled {
                    remote_call("prepare", "prepare_remote", json!({"source":"nci"}))
                } else {
                    remote_done()
                }
            } else if count == 2 {
                let receipt = find_remote_receipt(&body["input"])
                    .expect("native MCP prepare result must carry the generated receipt");
                let args = json!({"source":"nci", "correlationToken":receipt["correlationToken"],
                    "remoteName":receipt["suggestedJobName"], "remoteId":"123456.gadi-pbs",
                    "project":"zz00", "queue":"normal"});
                registration = Some(args.clone());
                remote_call("register", "register_remote", args)
            } else if count == 3 {
                let receipt =
                    find_registration_receipt(&body["input"]).expect("registration receipt");
                assert_eq!(receipt["deduplicated"], false);
                assert_eq!(receipt["job"]["state"], "registered");
                remote_call("retry", "register_remote", registration.clone().unwrap())
            } else {
                let receipt = find_registration_receipt(&body["input"]).expect("retry receipt");
                assert_eq!(receipt["deduplicated"], true);
                assert_eq!(receipt["job"]["state"], "registered");
                remote_done()
            };
            let events = [
                json!({"type":"response.created","response":{"id":format!("remote-{count}")}}),
                json!({"type":"response.output_item.done","item":item}),
                completed(&format!("remote-{count}")),
            ];
            let mut response = String::new();
            for event in events {
                response.push_str(&format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().unwrap()
                ));
            }
            write_http(&mut stream, "text/event-stream", response.as_bytes());
            count += 1;
        }
    });
    (format!("http://{address}"), stop_tx, worker)
}

fn remote_call(id: &str, name: &str, args: Value) -> Value {
    json!({"type":"function_call","call_id":format!("remote-{id}"),"namespace":"mcp__job_service",
        "name":name,"arguments":serde_json::to_string(&args).unwrap()})
}

fn remote_done() -> Value {
    json!({"type":"message","role":"assistant","id":"remote-done",
        "content":[{"type":"output_text","text":"remote probe complete"}]})
}

fn has_tool_name(value: &Value, expected: &str) -> bool {
    match value {
        Value::Object(fields) => {
            fields
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| name == expected || name.ends_with(&format!("__{expected}")))
                || fields.values().any(|value| has_tool_name(value, expected))
        }
        Value::Array(items) => items.iter().any(|value| has_tool_name(value, expected)),
        Value::String(text) => serde_json::from_str::<Value>(text)
            .ok()
            .is_some_and(|value| has_tool_name(&value, expected)),
        _ => false,
    }
}

struct ProbeProcess(std::process::Child, bool);

impl Drop for ProbeProcess {
    fn drop(&mut self) {
        if self.1 {
            unsafe {
                libc::kill(-(self.0.id() as i32), libc::SIGKILL);
            }
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn find_registration_receipt(value: &Value) -> Option<Value> {
    match value {
        Value::Object(fields) => {
            if value["deduplicated"].is_boolean() && value["job"].is_object() {
                return Some(value.clone());
            }
            fields.values().find_map(find_registration_receipt)
        }
        Value::Array(items) => items.iter().rev().find_map(find_registration_receipt),
        Value::String(text) => {
            json_fragments(text).find_map(|value| find_registration_receipt(&value))
        }
        _ => None,
    }
}

fn json_fragments(text: &str) -> impl Iterator<Item = Value> + '_ {
    text.char_indices()
        .filter(|(_, ch)| matches!(ch, '{' | '['))
        .filter_map(|(index, _)| {
            serde_json::Deserializer::from_str(&text[index..])
                .into_iter::<Value>()
                .next()?
                .ok()
        })
}

#[test]
fn mcp_stdio_discovery_retains_local_tools_when_core_is_unavailable() {
    let private_home =
        std::env::var_os("CUTEX_TEST_PRIVATE_HOME").expect("private test HOME required");
    assert_eq!(std::env::var_os("HOME"), Some(private_home.clone()));
    assert!(
        std::path::Path::new(&private_home)
            .join(".cutex-test-private-home")
            .is_file()
    );
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("missing.sock");
    let api = temp.path().join("api.key");
    let grant = temp.path().join("grant.key");
    write_secret(&api, API);
    write_secret(&grant, GRANT);
    let mut adapter = Command::new(binary())
        .args([
            "mcp-stdio",
            socket.to_str().unwrap(),
            api.to_str().unwrap(),
            grant.to_str().unwrap(),
            "/bin/false",
        ])
        .env("CUTEX_AGENT_ID", RUNTIME_ID)
        .env("CUTEX_AGENT_BUS_URL", "http://127.0.0.1:1")
        .env("CUTEX_AGENT_BUS_TOKEN", BUS_TOKEN)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = adapter.stdin.take().unwrap();
    for message in [
        json!({"jsonrpc":"2.0","id":1,"method":"server/discover"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    ] {
        serde_json::to_writer(&mut stdin, &message).unwrap();
        stdin.write_all(b"\n").unwrap();
    }
    drop(stdin);
    let output = adapter.wait_with_output().unwrap();
    assert!(output.status.success());
    let responses: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let result = &responses
        .iter()
        .find(|response| response["id"] == 2)
        .unwrap()["result"];
    assert_eq!(result["resultType"], "complete");
    let names: Vec<_> = result["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["submit", "query", "cancel", "read_output"]);
    assert!(!socket.exists());
}

/// The installed cute-codex CLI the tests run jobs through:
/// `CUTEX_JOB_TEST_LAUNCHER`, else `cute-codex` on PATH.
fn test_launcher() -> String {
    if let Some(path) = std::env::var_os("CUTEX_JOB_TEST_LAUNCHER").filter(|v| !v.is_empty()) {
        return path.to_string_lossy().into_owned();
    }
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("cute-codex"))
        .find(|path| path.is_file())
        .map(|path| path.to_string_lossy().into_owned())
        .expect("set CUTEX_JOB_TEST_LAUNCHER to an installed cute-codex CLI (or put cute-codex on PATH)")
}
