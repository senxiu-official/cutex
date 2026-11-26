use std::{
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

use serde_json::{json, Value};

use crate::{
    config::proxy_outbound::OutboundProxy,
    management::v2::codex_config,
    profiles::{materialize, model::StoredAccount},
    runtime::claude::ClaudeDeployment,
};

use super::super::{isolated_command, MAX_WIRE};
use super::{Answer, Failure, INSTRUCTIONS};

struct Transport {
    child: Child,
    input: std::process::ChildStdin,
    output: BufReader<std::process::ChildStdout>,
    log_disabled: mpsc::Receiver<()>,
}

impl Transport {
    fn spawn(command: &mut Command) -> Result<Self, Failure> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| Failure::ProfileUnavailable)?;
        let input = child.stdin.take().expect("piped stdin");
        let output = BufReader::new(child.stdout.take().expect("piped stdout"));
        let errors = child.stderr.take().expect("piped stderr");
        let (notify, log_disabled) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut errors = BufReader::new(errors);
            let mut bytes = Vec::new();
            loop {
                bytes.clear();
                match (&mut errors)
                    .take(MAX_WIRE + 1)
                    .read_until(b'\n', &mut bytes)
                {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                if serde_json::from_slice::<Value>(&bytes).is_ok_and(|event| {
                    event["target"] == "codex_app_server"
                        && event["fields"]["event.name"] == "diagnostic_database_unavailable"
                }) {
                    let _ = notify.try_send(());
                }
            }
        });
        Ok(Self {
            child,
            input,
            output,
            log_disabled,
        })
    }

    fn send(&mut self, value: Value) -> Result<(), Failure> {
        serde_json::to_writer(&mut self.input, &value).map_err(|_| Failure::Upstream)?;
        self.input.write_all(b"\n").map_err(|_| Failure::Upstream)
    }

    fn next(&mut self) -> Result<Value, Failure> {
        let mut bytes = Vec::new();
        (&mut self.output)
            .take(MAX_WIRE + 1)
            .read_until(b'\n', &mut bytes)
            .map_err(|_| Failure::Upstream)?;
        if bytes.is_empty() || bytes.len() as u64 > MAX_WIRE {
            return Err(Failure::Upstream);
        }
        let event: Value = serde_json::from_slice(&bytes).map_err(|_| Failure::Upstream)?;
        if event.get("id").is_some() && event.get("method").is_some() {
            return Err(Failure::InvalidOutput);
        }
        Ok(event)
    }

    fn request(&mut self, id: u64, method: &str, params: Value) -> Result<Value, Failure> {
        self.send(json!({"id":id,"method":method,"params":params}))?;
        loop {
            let event = self.next()?;
            if event["id"] == id {
                if let Some(error) = event.get("error") {
                    return Err(native_error(&error["data"]));
                }
                return event.get("result").cloned().ok_or(Failure::Upstream);
            }
        }
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn native_error(error: &Value) -> Failure {
    match error["codexErrorInfo"].as_str() {
        Some("usageLimitExceeded" | "sessionBudgetExceeded") => Failure::QuotaExhausted,
        Some("rateLimitExceeded") => Failure::RateLimited,
        Some("unauthorized") => Failure::Authentication,
        Some(
            "cyberPolicy" | "bioPolicy" | "misalignmentPolicyViolation" | "inferenceHookDenied",
        ) => Failure::ContentRefused,
        _ => Failure::Upstream,
    }
}

pub(super) fn native(
    profile: &StoredAccount,
    draft: &str,
    work: &Path,
    proxy: &OutboundProxy,
) -> Result<Answer, Failure> {
    // The pinned app-server supports a per-Profile auth file. The child uses
    // that existing auth store, including its normal refresh protocol.
    let files =
        materialize::materialized_account_files(profile).map_err(|_| Failure::Authentication)?;
    if !super::profile_quota_native::chatgpt_logged_in(profile) {
        return Err(Failure::Authentication);
    }
    let (program, args, _) =
        codex_config::resolve_runtime().map_err(|_| Failure::ProfileUnavailable)?;
    if !program.is_absolute() || args != ["--listen", "stdio://"] {
        return Err(Failure::ProfileUnavailable);
    }
    let mut command = isolated_command(&program, work, proxy);
    command
        .arg("--auth-file")
        .arg(&files.auth_path)
        .args([
            "-c",
            "cli_auth_credentials_store=\"file\"",
            "-c",
            "forced_login_method=\"chatgpt\"",
            "-c",
            "analytics.enabled=false",
            "-c",
            "model_max_output_tokens=6144",
            "-c",
            "project_doc_max_bytes=0",
        ])
        .args(args);
    native_turn(command, "openai", draft, work)
}

const NATIVE_LOG_DB: &str = "logs_2.sqlite";
const NATIVE_STATE_DB: &str = "state_5.sqlite";

fn native_turn(
    mut command: Command,
    model_provider: &str,
    draft: &str,
    work: &Path,
) -> Result<Answer, Failure> {
    const MODEL: &str = "gpt-5.6-luna";
    // r67 opens diagnostics separately from the required state databases.
    // SQLite cannot open a directory as a writable database. Native degrades
    // to no diagnostic log pool, while keeping the rest of the app-server live.
    let logs = work.join(NATIVE_LOG_DB);
    std::fs::create_dir(&logs).map_err(|_| Failure::ProfileUnavailable)?;
    command
        .env("CODEX_HOME", work)
        .env("CODEX_SQLITE_HOME", work)
        .env("LOG_FORMAT", "json")
        .env("RUST_LOG", "off,codex_app_server=warn");
    let start = Instant::now();
    let mut runtime = Transport::spawn(&mut command)?;
    runtime.request(
        1,
        "initialize",
        json!({"clientInfo":{"name":"cutex_text_rewrite","version":env!("CARGO_PKG_VERSION")},
                                             "capabilities":{"experimentalApi":true}}),
    )?;
    runtime.send(json!({"method":"initialized"}))?;
    let mut config = serde_json::Map::new();
    for feature in [
        "apps",
        "code_mode",
        "code_mode_only",
        "context_management",
        "current_time_reminder",
        "deferred_executor",
        "enable_fanout",
        "goals",
        "hooks",
        "image_generation",
        "memories",
        "multi_agent",
        "multi_agent_v2",
        "plugins",
        "request_permissions_tool",
        "shell_snapshot",
        "shell_tool",
        "standalone_web_search",
        "token_budget",
        "tool_suggest",
        "unified_exec",
        "view_image",
    ] {
        config.insert(format!("features.{feature}"), json!(false));
    }
    for key in [
        "cloud.skills.enabled",
        "skills.include_instructions",
        "tools.experimental_request_user_input.enabled",
        "tools.update_plan.enabled",
    ] {
        config.insert(key.into(), json!(false));
    }
    config.insert("web_search".into(), json!("disabled"));
    config.insert("default_permissions".into(), json!(":read-only"));
    let started = runtime.request(
        2,
        "thread/start",
        json!({"cwd":work,"model":MODEL,"modelProvider":model_provider,
        "ephemeral":true,"approvalPolicy":"never","sandbox":"read-only","environments":[],
        "runtimeWorkspaceRoots":[],"dynamicTools":[],"selectedCapabilityRoots":[],
        "baseInstructions":INSTRUCTIONS,"config":config}),
    )?;
    if started.pointer("/thread/ephemeral") != Some(&Value::Bool(true))
        || started.pointer("/sandbox/type").and_then(Value::as_str) != Some("readOnly")
    {
        return Err(Failure::ProfileUnavailable);
    }
    // The expected state layout confirms the pinned SQLite path before the
    // draft crosses stdin. Reject a changed layout instead of risking logs.
    verify_native_log_block(work, &logs)?;
    runtime
        .log_disabled
        .recv_timeout(Duration::from_millis(500))
        .map_err(|_| Failure::ProfileUnavailable)?;
    let thread = started["thread"]["id"].as_str().ok_or(Failure::Upstream)?;
    let startup_ms = Some(start.elapsed().as_millis());
    let turn = runtime.request(3, "turn/start",
        json!({"threadId":thread,"input":[{"type":"text","text":draft,"text_elements":[]}],"effort":"low"}))?;
    let turn_id = turn["turn"]["id"].as_str().ok_or(Failure::Upstream)?;
    let mut last_text = None;
    loop {
        let event = runtime.next()?;
        let params = &event["params"];
        if params["threadId"] != thread {
            continue;
        }
        if params
            .get("turnId")
            .is_some_and(|id| id.as_str() != Some(turn_id))
        {
            continue;
        }
        match event["method"].as_str() {
            Some("item/started" | "item/completed") => match params["item"]["type"].as_str() {
                Some("agentMessage") if event["method"] == "item/completed" => {
                    last_text = params["item"]["text"].as_str().map(str::to_owned)
                }
                Some("agentMessage" | "userMessage" | "reasoning") => {}
                _ => return Err(Failure::InvalidOutput),
            },
            Some("turn/completed") if params["turn"]["id"] == turn_id => {
                if params["turn"]["status"] != "completed" {
                    return Err(native_error(&params["turn"]["error"]));
                }
                return Ok(Answer {
                    text: last_text.ok_or(Failure::InvalidOutput)?,
                    model: started["model"].as_str().map(str::to_owned),
                    requested_model: MODEL.into(),
                    provider: "openai".into(),
                    startup_ms,
                });
            }
            _ => {}
        }
    }
}

fn verify_native_log_block(work: &Path, logs: &Path) -> Result<(), Failure> {
    use std::fs;
    let entries = fs::read_dir(work).map_err(|_| Failure::ProfileUnavailable)?;
    let blocker_empty = fs::read_dir(logs)
        .map_err(|_| Failure::ProfileUnavailable)?
        .next()
        .is_none();
    if !blocker_empty || !work.join(NATIVE_STATE_DB).is_file() {
        return Err(Failure::ProfileUnavailable);
    }
    for entry in entries {
        let entry = entry.map_err(|_| Failure::ProfileUnavailable)?;
        if entry.file_name().to_string_lossy().starts_with("logs") && entry.path() != logs {
            return Err(Failure::ProfileUnavailable);
        }
    }
    Ok(())
}

fn credential_path(home: &Path) -> Option<PathBuf> {
    let file = home.join(".credentials.json");
    let meta = std::fs::symlink_metadata(&file).ok()?;
    (meta.is_file() && meta.len() <= 128 * 1024).then_some(file)
}

pub(super) fn claude(
    deployment: &ClaudeDeployment,
    home: &Path,
    draft: &str,
    work: &Path,
    proxy: &OutboundProxy,
) -> Result<Answer, Failure> {
    // Borrow an access token only. A throwaway config must not independently
    // refresh a copy of the owner's existing rotating subscription credential.
    let file = credential_path(home).ok_or(Failure::Authentication)?;
    let credentials: Value =
        serde_json::from_slice(&std::fs::read(file).map_err(|_| Failure::Authentication)?)
            .map_err(|_| Failure::Authentication)?;
    let oauth = &credentials["claudeAiOauth"];
    if oauth["expiresAt"]
        .as_i64()
        .is_some_and(|at| at <= chrono::Utc::now().timestamp_millis())
    {
        return Err(Failure::Authentication);
    }
    let token = crate::profiles::stored_keys::SecretKey::new(
        oauth["accessToken"]
            .as_str()
            .ok_or(Failure::Authentication)?,
    )
    .map_err(|_| Failure::Authentication)?;
    let mut child = isolated_command(&deployment.python, work, proxy)
        .args(["-I", "-B", "-c", include_str!("claude_query.py")])
        .arg(&deployment.claude_cli)
        .env("CLAUDE_CONFIG_DIR", work)
        .env("CLAUDE_CODE_OAUTH_TOKEN", token.expose())
        .env("CLAUDE_CODE_SKIP_PROMPT_HISTORY", "1")
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .env("DISABLE_TELEMETRY", "1")
        .env("DISABLE_ERROR_REPORTING", "1")
        .env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "6144")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| Failure::ProfileUnavailable)?;
    let result = (|| {
        let input = child.stdin.take().expect("piped stdin");
        serde_json::to_writer(input, &json!({"system":INSTRUCTIONS,"draft":draft}))
            .map_err(|_| Failure::Upstream)?;
        let mut bytes = Vec::new();
        child
            .stdout
            .take()
            .expect("piped stdout")
            .take(MAX_WIRE + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Failure::Upstream)?;
        if bytes.len() as u64 > MAX_WIRE {
            return Err(Failure::InvalidOutput);
        }
        serde_json::from_slice::<Result<Answer, Failure>>(&bytes).map_err(|_| Failure::Upstream)?
    })();
    let _ = child.kill();
    let _ = child.wait();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::server::read_simple_http_request;
    use std::{fs, io, net::TcpListener, thread};

    fn assert_no_payload(path: &Path, markers: &[&str]) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                assert_no_payload(&entry.path(), markers);
            } else {
                let bytes = fs::read(entry.path()).unwrap();
                for marker in markers {
                    assert!(
                        !bytes
                            .windows(marker.len())
                            .any(|window| window == marker.as_bytes()),
                        "payload in {:?}",
                        entry.file_name()
                    );
                }
            }
        }
    }

    #[test]
    #[ignore = "requires CUTEX_TEST_NATIVE_REWRITE_BIN pointing to the pinned Native app-server"]
    fn pinned_native_does_not_persist_input_or_output() {
        let image = PathBuf::from(
            std::env::var_os("CUTEX_TEST_NATIVE_REWRITE_BIN").expect("pinned Native binary path"),
        );
        assert!(image.is_absolute() && image.is_file());
        let home = super::super::super::scratch::TestDir::new();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        const DRAFT: &str = "CUTEX_PRIVATE_NATIVE_DRAFT_F223A901";
        const REWRITE: &str = "CUTEX_PRIVATE_NATIVE_REWRITE_97B607A8";
        let text = json!({"rewrittenText": REWRITE, "missingDetails": []}).to_string();
        let response = text.clone();
        listener.set_nonblocking(true).unwrap();
        let upstream = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(12);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error)
                        if error.kind() == io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(10))
                    }
                    Err(error) => panic!("loopback model received no native request: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(4)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(4)))
                .unwrap();
            let request = read_simple_http_request(&mut stream).unwrap();
            assert_eq!(request.path, "/v1/responses");
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert!(body["input"].to_string().contains(DRAFT));
            assert!(body
                .get("tools")
                .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty)));
            let part = json!({"type":"output_text","text":response,"annotations":[]});
            let item =
                json!({"type":"message","id":"msg_fixture","role":"assistant","content":[part]});
            let events = [
                json!({"type":"response.created","response":{"id":"resp_fixture"}}),
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_fixture","role":"assistant","content":[]}}),
                json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":"msg_fixture","delta":response}),
                json!({"type":"response.output_item.done","output_index":0,"item": item}),
                json!({"type":"response.completed","response":{"id":"resp_fixture","status":"completed","output":[item],"usage":{"input_tokens":12,"output_tokens":10,"total_tokens":22}}}),
            ];
            let payload = events
                .iter()
                .map(|event| {
                    format!(
                        "event: {}\r\ndata: {}\r\n\r\n",
                        event["type"].as_str().unwrap(),
                        event
                    )
                })
                .collect::<String>();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", payload.len(), payload).unwrap();
        });
        let mut command = isolated_command(&image, &home.0, &OutboundProxy::Direct);
        command
            .env("CUTEX_REWRITE_FIXTURE_KEY", "not-a-real-key")
            .args([
                "-c",
                "cli_auth_credentials_store=\"file\"",
                "-c",
                "analytics.enabled=false",
                "-c",
                "project_doc_max_bytes=0",
            ])
            .arg("-c")
            .arg(format!(
                "model_providers.fixture.base_url=\"http://127.0.0.1:{port}/v1\""
            ))
            .args([
                "-c",
                "model_providers.fixture.name=\"Fixture\"",
                "-c",
                "model_providers.fixture.wire_api=\"responses\"",
                "-c",
                "model_providers.fixture.env_key=\"CUTEX_REWRITE_FIXTURE_KEY\"",
                "--listen",
                "stdio://",
            ]);
        let answer = native_turn(command, "fixture", DRAFT, &home.0);
        upstream.join().unwrap();
        assert_eq!(answer.unwrap().text, text);
        assert!(home.0.join(NATIVE_LOG_DB).is_dir());
        assert_no_payload(&home.0, &[DRAFT, REWRITE]);
    }
}
