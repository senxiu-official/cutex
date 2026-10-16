use crate::{
    CallerGrantIssuer, CallerOperation, ExecutionOrigin, GrantIssuer, JobError, JobRequest,
    RemoteJobRequest, TrustedSandboxContext,
};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const SANDBOX_META_KEY: &str = "codex/sandbox-state-meta";
// URI query values retain RFC 3986 unreserved characters. In particular,
// Cutex's raw requester parser must see generated stock.<UUID> IDs unchanged.
const QUERY_VALUE_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

#[derive(Debug, Clone)]
pub struct McpAdapterConfig {
    pub socket_path: PathBuf,
    pub api_token: Vec<u8>,
    pub grant_key: Vec<u8>,
    pub launcher_path: String,
    pub launcher_sha256: String,
}

#[derive(Debug, Clone)]
struct RuntimeBinding {
    runtime_agent_id: String,
    agent_bus_url: String,
    agent_bus_token: String,
}

#[derive(Debug, Deserialize)]
struct AgentProjection {
    id: String,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default, alias = "cutexSessionId")]
    cutex_session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SubmitArgs {
    action_id: String,
    argv: Vec<String>,
    cwd: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PrepareRemoteArgs {
    source: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct JobArgs {
    job_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CancelArgs {
    job_id: String,
    expected_revision: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReadOutputArgs {
    job_id: String,
    stream: String,
    #[serde(default)]
    offset: u64,
    #[serde(default = "default_read_size")]
    max_bytes: usize,
}

pub fn serve_mcp_stdio(config: McpAdapterConfig) -> Result<(), JobError> {
    validate_config(&config)?;
    let binding = RuntimeBinding::from_environment()?;
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    let mut modern = false;
    for line in stdin.lock().lines() {
        let line = line?;
        if line.len() > MAX_MESSAGE_BYTES {
            return Err(JobError::Invalid("MCP request exceeds 1 MiB".into()));
        }
        let message: Value = serde_json::from_str(&line)?;
        if message.get("method").and_then(Value::as_str) == Some("notifications/initialized")
            || message.get("id").is_none()
        {
            continue;
        }
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        if message.get("method").and_then(Value::as_str) == Some("server/discover") {
            modern = true;
        }
        let response = match dispatch_mcp(&config, &binding, &message, modern) {
            Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
            Err(error) => json!({
                "jsonrpc":"2.0",
                "id":id,
                "error":{"code":-32602,"message":error.to_string()}
            }),
        };
        serde_json::to_writer(&mut stdout, &response)?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }
    Ok(())
}

fn dispatch_mcp(
    config: &McpAdapterConfig,
    binding: &RuntimeBinding,
    message: &Value,
    modern: bool,
) -> Result<Value, JobError> {
    match message.get("method").and_then(Value::as_str) {
        Some("server/discover") => Ok(json!({
            "resultType":"complete",
            "supportedVersions":["2026-07-28"],
            "capabilities":{
                "tools":{},
                "experimental":{SANDBOX_META_KEY:{}}
            },
            "instructions":"Submit noninteractive jobs. completionDelivery.enabled reports channel configuration; awaiting_terminal means the job is still running, not notifications disabled. Completion events arrive after the current turn. Read/query do not consume notifications; do not poll.",
            "ttlMs":0,
            "cacheScope":"private",
            "_meta":{"io.modelcontextprotocol/serverInfo":{"name":"cutex-job-service","version":env!("CARGO_PKG_VERSION")}}
        })),
        Some("initialize") => {
            let version = message
                .pointer("/params/protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("2025-06-18");
            Ok(json!({
                "protocolVersion":version,
                "capabilities":{
                    "tools":{"listChanged":false},
                    "experimental":{SANDBOX_META_KEY:{}}
                },
                "serverInfo":{"name":"cutex-job-service","version":env!("CARGO_PKG_VERSION")},
                "instructions":"Submit noninteractive jobs. completionDelivery.enabled reports channel configuration; awaiting_terminal means the job is still running, not notifications disabled. Completion events arrive after the current turn. Read/query do not consume notifications; do not poll."
            }))
        }
        Some("tools/list") => {
            // Local tools remain discoverable while the optional source probe is unavailable.
            let capabilities = core_call(config, "capabilities", json!({})).unwrap_or(Value::Null);
            let mut result = json!({"tools":tools(&capabilities)});
            if modern {
                result["resultType"] = json!("complete");
                result["ttlMs"] = json!(0);
                result["cacheScope"] = json!("private");
            }
            Ok(result)
        }
        Some("tools/call") => call_tool(config, binding, message, modern),
        Some(_) | None => Err(JobError::Invalid("unsupported MCP method".into())),
    }
}

fn call_tool(
    config: &McpAdapterConfig,
    binding: &RuntimeBinding,
    message: &Value,
    modern: bool,
) -> Result<Value, JobError> {
    let params = message
        .get("params")
        .and_then(Value::as_object)
        .ok_or_else(|| JobError::Invalid("tools/call params are required".into()))?;
    let metadata = params
        .get("_meta")
        .and_then(Value::as_object)
        .ok_or_else(|| JobError::Unauthorized("trusted MCP request metadata is missing".into()))?;
    let subject = binding.resolve_subject(metadata)?;
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| JobError::Invalid("tool name is required".into()))?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let result = match name {
        "submit" => {
            let args: SubmitArgs = serde_json::from_value(arguments)?;
            let sandbox_state = metadata.get(SANDBOX_META_KEY).cloned().ok_or_else(|| {
                JobError::Unauthorized("originating sandbox metadata is missing".into())
            })?;
            let request = JobRequest {
                action_id: args.action_id,
                argv: args.argv,
                cwd: args.cwd,
                environment: BTreeMap::new(),
                subscriber_cutex_session_id: subject.clone(),
                origin: ExecutionOrigin {
                    runtime_agent_id: binding.runtime_agent_id.clone(),
                    native_thread_id: metadata
                        .get("threadId")
                        .and_then(Value::as_str)
                        .expect("resolve_subject validated threadId")
                        .to_string(),
                    permission_profile_type: permission_profile_type(&sandbox_state)?.to_string(),
                },
            };
            let grant = GrantIssuer::new(&config.grant_key)?.issue(
                &request,
                TrustedSandboxContext {
                    subject_cutex_session_id: subject,
                    cwd: request.cwd.clone(),
                    sandbox_state,
                    launcher_path: config.launcher_path.clone(),
                    launcher_sha256: config.launcher_sha256.clone(),
                    operating_system_uid: unsafe { libc::geteuid() },
                },
                now_secs(),
                60,
            )?;
            core_call(config, "submit", json!({"request":request,"grant":grant}))?
        }
        "prepare_remote" => {
            let args: PrepareRemoteArgs = serde_json::from_value(arguments)?;
            caller_core_call(
                config,
                subject,
                CallerOperation::PrepareRemote,
                &args.source,
                json!({"source":args.source}),
            )?
        }
        "register_remote" => {
            let args: RemoteJobRequest = serde_json::from_value(arguments)?;
            caller_core_call(
                config,
                subject,
                CallerOperation::RegisterRemote,
                &crate::remote_request_sha256(&args)?,
                json!({"request":args}),
            )?
        }
        "query" => {
            let args: JobArgs = serde_json::from_value(arguments)?;
            caller_core_call(
                config,
                subject,
                CallerOperation::Query,
                &args.job_id,
                json!({}),
            )?
        }
        "cancel" => {
            let args: CancelArgs = serde_json::from_value(arguments)?;
            caller_core_call(
                config,
                subject,
                CallerOperation::Cancel,
                &args.job_id,
                json!({"expectedRevision":args.expected_revision}),
            )?
        }
        "read_output" => {
            let args: ReadOutputArgs = serde_json::from_value(arguments)?;
            caller_core_call(
                config,
                subject,
                CallerOperation::ReadOutput,
                &args.job_id,
                json!({"stream":args.stream,"offset":args.offset,"maxBytes":args.max_bytes.clamp(1, 8192)}),
            )?
        }
        _ => return Err(JobError::Invalid("unknown Job Service tool".into())),
    };
    let result = if name == "read_output" {
        readable_output(result)?
    } else {
        result
    };
    let mut response = json!({
        "content":[{"type":"text","text":serde_json::to_string(&result)?}],
        "structuredContent":result,
        "isError":false
    });
    if modern {
        response["resultType"] = json!("complete");
    }
    Ok(response)
}

pub(crate) fn readable_output(mut result: Value) -> Result<Value, JobError> {
    if let Some(object) = result.as_object_mut() {
        if let Some(encoded) = object.remove("bytesHex") {
            let bytes = hex::decode(encoded.as_str().unwrap_or_default())
                .map_err(|_| JobError::Invalid("invalid output encoding".into()))?;
            let from = object
                .get("fromOffset")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let mut consumed = bytes.len();
            loop {
                // Keep valid multibyte characters whole, and original-byte offsets exact.
                if let Err(error) = std::str::from_utf8(&bytes[..consumed]) {
                    if error.error_len().is_none() && error.valid_up_to() > 0 {
                        consumed = error.valid_up_to();
                    }
                }
                object.insert("nextOffset".into(), json!(from + consumed as u64));
                object.insert(
                    "text".into(),
                    json!(String::from_utf8_lossy(&bytes[..consumed])),
                );
                object.insert("encoding".into(), json!("utf-8-lossy"));
                object.insert("pageLimited".into(), json!(consumed < bytes.len()));
                if serde_json::to_vec(&object)?.len() <= 12 * 1024 || consumed <= 1 {
                    break;
                }
                consumed /= 2;
            }
        }
    }
    Ok(result)
}

fn caller_core_call(
    config: &McpAdapterConfig,
    subject: String,
    operation: CallerOperation,
    job_id: &str,
    additions: Value,
) -> Result<Value, JobError> {
    let grant = CallerGrantIssuer::new(&config.grant_key)?.issue(
        subject,
        operation,
        job_id.to_string(),
        now_secs(),
        60,
    )?;
    let mut params = additions.as_object().cloned().unwrap_or_default();
    params.insert("jobId".into(), Value::String(job_id.to_string()));
    params.insert("callerGrant".into(), serde_json::to_value(grant)?);
    let method = match operation {
        CallerOperation::Query => "query",
        CallerOperation::Cancel => "cancel",
        CallerOperation::ReadOutput => "readOutput",
        CallerOperation::PrepareRemote => "prepareRemote",
        CallerOperation::RegisterRemote => "registerRemote",
    };
    core_call(config, method, Value::Object(params))
}

fn core_call(config: &McpAdapterConfig, method: &str, params: Value) -> Result<Value, JobError> {
    let mut stream = UnixStream::connect(&config.socket_path)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let request = json!({
        "token":hex::encode(&config.api_token),
        "method":method,
        "params":params,
    });
    serde_json::to_writer(&mut stream, &request)?;
    stream.write_all(b"\n")?;
    let mut line = String::new();
    BufReader::new(stream)
        .take((MAX_MESSAGE_BYTES + 1) as u64)
        .read_line(&mut line)?;
    if line.len() > MAX_MESSAGE_BYTES {
        return Err(JobError::Invalid(
            "Job Service response exceeds 1 MiB".into(),
        ));
    }
    let response: Value = serde_json::from_str(&line)?;
    if response.get("ok").and_then(Value::as_bool) == Some(true) {
        return response
            .get("result")
            .cloned()
            .ok_or_else(|| JobError::Invalid("Job Service response omitted result".into()));
    }
    Err(JobError::Unauthorized(
        response
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Job Service rejected request")
            .to_string(),
    ))
}

impl RuntimeBinding {
    fn from_environment() -> Result<Self, JobError> {
        Ok(Self {
            runtime_agent_id: required_env("CUTEX_AGENT_ID")?,
            agent_bus_url: required_env("CUTEX_AGENT_BUS_URL")?,
            agent_bus_token: required_env("CUTEX_AGENT_BUS_TOKEN")?,
        })
    }

    fn resolve_subject(
        &self,
        metadata: &serde_json::Map<String, Value>,
    ) -> Result<String, JobError> {
        let metadata_thread = metadata
            .get("threadId")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                JobError::Unauthorized("trusted MCP thread metadata is missing".into())
            })?;
        let agents = fetch_agent_projection(
            &self.agent_bus_url,
            &self.agent_bus_token,
            &self.runtime_agent_id,
        )?;
        let mut exact = agents
            .into_iter()
            .filter(|agent| agent.id == self.runtime_agent_id);
        let agent = exact
            .next()
            .ok_or_else(|| JobError::Unauthorized("runtime occurrence is not registered".into()))?;
        if exact.next().is_some() || agent.session_id.as_deref() != Some(metadata_thread) {
            return Err(JobError::Unauthorized(
                "runtime occurrence/thread binding is ambiguous or stale".into(),
            ));
        }
        agent
            .cutex_session_id
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                JobError::Unauthorized("runtime has no current durable session projection".into())
            })
    }
}

fn fetch_agent_projection(
    base_url: &str,
    token: &str,
    runtime_agent_id: &str,
) -> Result<Vec<AgentProjection>, JobError> {
    let address = base_url
        .strip_prefix("http://127.0.0.1:")
        .filter(|value| value.bytes().all(|byte| byte.is_ascii_digit()))
        .ok_or_else(|| JobError::Unauthorized("Agent Bus URL must be IPv4 loopback HTTP".into()))?;
    let port: u16 = address
        .parse()
        .map_err(|_| JobError::Unauthorized("Agent Bus port is invalid".into()))?;
    let path = format!(
        "/api/agents?agent_id={}&all_hosts=false",
        utf8_percent_encode(runtime_agent_id, QUERY_VALUE_ENCODE_SET)
    );
    let mut stream = TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}")
            .parse()
            .map_err(|_| JobError::Unauthorized("Agent Bus address is invalid".into()))?,
        Duration::from_secs(2),
    )?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    )?;
    let mut bytes = Vec::new();
    stream
        .take((MAX_MESSAGE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(JobError::Unauthorized(
            "Agent Bus response exceeds 1 MiB".into(),
        ));
    }
    let split = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| JobError::Unauthorized("Agent Bus response is malformed".into()))?;
    let head = std::str::from_utf8(&bytes[..split])
        .map_err(|_| JobError::Unauthorized("Agent Bus headers are malformed".into()))?;
    if !head
        .lines()
        .next()
        .is_some_and(|line| line.contains(" 200 "))
    {
        return Err(JobError::Unauthorized(
            "Agent Bus rejected identity lookup".into(),
        ));
    }
    serde_json::from_slice(&bytes[split + 4..]).map_err(JobError::from)
}

fn tools(capabilities: &Value) -> Vec<Value> {
    let mut tools = vec![
        tool(
            "submit",
            "Submit one noninteractive job and return its durable receipt immediately. completionDelivery.enabled is channel configuration; state=awaiting_terminal means completion has not occurred yet. Completion is delivered after the current turn.",
            json!({"type":"object","properties":{"actionId":{"type":"string"},"argv":{"type":"array","items":{"type":"string"}},"cwd":{"type":"string"}},"required":["actionId","argv","cwd"],"additionalProperties":false}),
        ),
        tool(
            "query",
            "Read one job owned by the authenticated originating Agent; do not poll.",
            job_schema(),
        ),
        tool(
            "cancel",
            "Cancel one owned job using its current revision.",
            json!({"type":"object","properties":{"jobId":{"type":"string"},"expectedRevision":{"type":"integer","minimum":1}},"required":["jobId","expectedRevision"],"additionalProperties":false}),
        ),
        tool(
            "read_output",
            "Read UTF-8 text (default/max 8192 original bytes). Follow nextOffset in original stream bytes; gap/omittedBytes report discarded middle output. Invalid UTF-8 is replaced for display. Completed oversized streams retain head and tail. Reads do not consume completion notifications.",
            json!({"type":"object","properties":{"jobId":{"type":"string"},"stream":{"type":"string","enum":["stdout","stderr"]},"offset":{"type":"integer","minimum":0},"maxBytes":{"type":"integer","minimum":1,"maximum":8192}},"required":["jobId","stream"],"additionalProperties":false}),
        ),
    ];
    let sources: Vec<&str> = capabilities
        .get("sources")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if !sources.is_empty() {
        tools.push(tool("prepare_remote",
            "Prepare registration of an external job owned by this Agent; does not submit any job. Returns a correlation token and `pbsArgs`: pass `pbsArgs` verbatim to qsub (job name, mail events and, when the owner configured it, the `-M` notification address). Never add, change or ask for a mail address yourself. If `notificationStatus` is not `configured`, still submit with `pbsArgs` when authorized and relay `ownerAction` to the owner. Then call register_remote with the PBS job id.",
            json!({"type":"object","properties":{"source":{"type":"string","enum":sources}},"required":["source"],"additionalProperties":false})));
        tools.push(tool("register_remote",
            "Register a job you already submitted externally using your prepared correlation token. Creates registered state only; completion is reported by trusted sources or the owner.",
            json!({"type":"object","properties":{"source":{"type":"string","enum":sources},"correlationToken":{"type":"string"},"remoteId":{"type":"string"},"remoteName":{"type":"string"},"project":{"type":"string"},"queue":{"type":"string"},"remoteOutputPath":{"type":"string"}},"required":["source","correlationToken","remoteId"],"additionalProperties":false})));
    }
    tools
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({
        "name":name,
        "description":description,
        "inputSchema":input_schema,
        "annotations":{"readOnlyHint":name == "query" || name == "read_output"}
    })
}

fn job_schema() -> Value {
    json!({"type":"object","properties":{"jobId":{"type":"string"}},"required":["jobId"],"additionalProperties":false})
}

fn required_env(name: &str) -> Result<String, JobError> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| JobError::Unauthorized(format!("trusted runtime environment omits {name}")))
}

fn permission_profile_type(sandbox_state: &Value) -> Result<&str, JobError> {
    sandbox_state
        .pointer("/permissionProfile/type")
        .and_then(Value::as_str)
        .ok_or_else(|| JobError::Unauthorized("originating permission profile is missing".into()))
}

fn validate_config(config: &McpAdapterConfig) -> Result<(), JobError> {
    if !config.socket_path.is_absolute()
        || config.api_token.len() < 32
        || config.grant_key.len() < 32
        || config.launcher_sha256.len() != 64
    {
        return Err(JobError::Invalid(
            "MCP adapter configuration is invalid".into(),
        ));
    }
    Ok(())
}

fn default_read_size() -> usize {
    8 * 1024
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn discovery_config(socket_path: PathBuf) -> McpAdapterConfig {
        McpAdapterConfig {
            socket_path,
            api_token: vec![8; 32],
            grant_key: vec![7; 32],
            launcher_path: "/unused".into(),
            launcher_sha256: "0".repeat(64),
        }
    }

    fn discovery_binding() -> RuntimeBinding {
        RuntimeBinding {
            runtime_agent_id: "fixture".into(),
            agent_bus_url: "http://127.0.0.1:1".into(),
            agent_bus_token: "fixture".into(),
        }
    }

    fn assert_local_discovery(result: &Value) {
        let names: Vec<_> = result["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["submit", "query", "cancel", "read_output"]);
    }

    #[test]
    fn tools_list_falls_back_to_local_tools_when_socket_is_missing() {
        let root = tempfile::tempdir().unwrap();
        let config = discovery_config(root.path().join("missing.sock"));
        for modern in [false, true] {
            let result = dispatch_mcp(
                &config,
                &discovery_binding(),
                &json!({"method":"tools/list"}),
                modern,
            )
            .unwrap();
            assert_local_discovery(&result);
            if modern {
                assert_eq!(result["resultType"], "complete");
                assert_eq!(result["cacheScope"], "private");
                assert_eq!(result["ttlMs"], 0);
            }
        }
    }

    #[test]
    fn tools_list_falls_back_on_rejected_or_malformed_capability_response() {
        use std::os::unix::net::UnixListener;
        for reply in [
            b"{\"ok\":false,\"message\":\"unavailable\"}\n".as_slice(),
            b"not-json\n",
        ] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("probe.sock");
            let listener = UnixListener::bind(&path).unwrap();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["method"], "capabilities");
                stream.write_all(reply).unwrap();
            });
            let result = dispatch_mcp(
                &discovery_config(path),
                &discovery_binding(),
                &json!({"method":"tools/list"}),
                false,
            )
            .unwrap();
            assert_local_discovery(&result);
            server.join().unwrap();
        }
    }

    #[test]
    fn remote_tools_require_an_enabled_source_and_offer_no_marking() {
        for capabilities in [json!({}), json!({"sources":[]})] {
            let names: Vec<Value> = tools(&capabilities)
                .iter()
                .map(|t| t["name"].clone())
                .collect();
            assert_eq!(
                names,
                vec![
                    json!("submit"),
                    json!("query"),
                    json!("cancel"),
                    json!("read_output")
                ]
            );
        }
        let enabled = tools(&json!({"sources":["nci"]}));
        assert_eq!(enabled.len(), 6);
        for (tool, name) in enabled[4..]
            .iter()
            .zip(["prepare_remote", "register_remote"])
        {
            assert_eq!(tool["name"], name);
            assert_eq!(
                tool["inputSchema"]["properties"]["source"]["enum"],
                json!(["nci"])
            );
            assert_eq!(tool["inputSchema"]["additionalProperties"], false);
            assert!(tool["inputSchema"]["properties"].get("state").is_none());
            assert!(
                tool["inputSchema"]["properties"]
                    .get("subscriberCutexSessionId")
                    .is_none()
            );
        }
    }

    #[test]
    fn remote_calls_bind_trusted_subject_operation_and_registration_token() {
        use std::os::unix::net::UnixListener;
        for (operation, method, binding) in [
            (CallerOperation::PrepareRemote, "prepareRemote", "nci"),
            (
                CallerOperation::RegisterRemote,
                "registerRemote",
                "prepared-token",
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("adapter.sock");
            let listener = UnixListener::bind(&path).unwrap();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["method"], method);
                assert_eq!(request["params"]["jobId"], binding);
                let grant: crate::CallerGrant =
                    serde_json::from_value(request["params"]["callerGrant"].clone()).unwrap();
                crate::grant::verify_caller_grant(
                    &[7; 32],
                    &grant,
                    operation,
                    binding,
                    "trusted-session",
                    now_secs(),
                )
                .unwrap();
                assert!(
                    crate::grant::verify_caller_grant(
                        &[7; 32],
                        &grant,
                        operation,
                        binding,
                        "foreign-session",
                        now_secs()
                    )
                    .is_err()
                );
                stream
                    .write_all(b"{\"ok\":true,\"result\":{\"registered\":true}}\n")
                    .unwrap();
            });
            let config = McpAdapterConfig {
                socket_path: path,
                api_token: vec![8; 32],
                grant_key: vec![7; 32],
                launcher_path: "/unused".into(),
                launcher_sha256: "0".repeat(64),
            };
            let result = caller_core_call(
                &config,
                "trusted-session".into(),
                operation,
                binding,
                json!({}),
            )
            .unwrap();
            assert_eq!(result["registered"], true);
            server.join().unwrap();
        }
    }

    #[test]
    fn registration_grant_rejects_mutated_remote_fields() {
        let request: RemoteJobRequest = serde_json::from_value(json!({
            "source":"nci", "correlationToken":"0123456789abcdef0123456789abcdef",
            "remoteId":"123.gadi-pbs", "remoteOutputPath":"/scratch/project/results"
        }))
        .unwrap();
        let binding = crate::remote_request_sha256(&request).unwrap();
        let grant = CallerGrantIssuer::new(&[7; 32])
            .unwrap()
            .issue(
                "trusted-session".into(),
                CallerOperation::RegisterRemote,
                binding.clone(),
                now_secs(),
                60,
            )
            .unwrap();
        crate::grant::verify_caller_grant(
            &[7; 32],
            &grant,
            CallerOperation::RegisterRemote,
            &binding,
            "trusted-session",
            now_secs(),
        )
        .unwrap();
        for field in [
            "source",
            "correlationToken",
            "remoteId",
            "remoteName",
            "project",
            "queue",
            "remoteOutputPath",
        ] {
            let mut mutated = serde_json::to_value(&request).unwrap();
            mutated[field] = json!("changed");
            let mutated: RemoteJobRequest = serde_json::from_value(mutated).unwrap();
            assert!(
                crate::grant::verify_caller_grant(
                    &[7; 32],
                    &grant,
                    CallerOperation::RegisterRemote,
                    &crate::remote_request_sha256(&mutated).unwrap(),
                    "trusted-session",
                    now_secs()
                )
                .is_err()
            );
        }
    }

    #[test]
    fn remote_arguments_reject_model_status_or_subscriber_overrides() {
        assert!(
            serde_json::from_value::<PrepareRemoteArgs>(
                json!({"source":"nci","subscriberCutexSessionId":"other"})
            )
            .is_err()
        );
        for extra in ["state", "exitCode", "subscriberCutexSessionId"] {
            let mut args =
                json!({"source":"nci","correlationToken":"abc","remoteId":"123.gadi-pbs"});
            args[extra] = json!("forged");
            assert!(serde_json::from_value::<RemoteJobRequest>(args).is_err());
        }
    }

    #[test]
    fn requester_query_preserves_unreserved_runtime_ids() {
        for id in [
            "stock.3a71b74b-253f-4701-b99b-6a7e6a29af28",
            "cutex.agent_name.project-1.012345abcd",
            "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~",
        ] {
            assert_eq!(
                utf8_percent_encode(id, QUERY_VALUE_ENCODE_SET).to_string(),
                id
            );
        }
    }

    #[test]
    fn requester_query_encodes_reserved_and_non_ascii_values() {
        for (input, expected) in [
            ("%&= +/?#", "%25%26%3D%20%2B%2F%3F%23"),
            ("私有", "%E7%A7%81%E6%9C%89"),
            ("runtime&all_hosts=true", "runtime%26all_hosts%3Dtrue"),
            ("\r\n", "%0D%0A"),
        ] {
            assert_eq!(
                utf8_percent_encode(input, QUERY_VALUE_ENCODE_SET).to_string(),
                expected
            );
        }
    }

    #[test]
    fn requester_query_raw_http_and_exact_identity_selection() {
        use std::net::TcpListener;
        let id = "stock.3a71b74b-253f-4701-b99b-6a7e6a29af28";
        let valid =
            json!({"id":id,"session_id":"native-private","cutex_session_id":"cutex.private"});
        for (rows, accepted) in [
            (json!([valid.clone()]), true),
            (json!([]), false),
            (json!([valid.clone(), valid.clone()]), false),
            (
                json!([{"id":id,"session_id":"foreign","cutex_session_id":"cutex.private"}]),
                false,
            ),
            (
                json!([{"id":id,"session_id":"native-private","cutex_session_id":null}]),
                false,
            ),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                    assert!(request.len() < 4096);
                }
                assert!(String::from_utf8(request).unwrap().starts_with(&format!(
                    "GET /api/agents?agent_id={id}&all_hosts=false HTTP/1.1\r\n"
                )));
                let body = serde_json::to_vec(&rows).unwrap();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(&body).unwrap();
            });
            let binding = RuntimeBinding {
                runtime_agent_id: id.into(),
                agent_bus_url: format!("http://{address}"),
                agent_bus_token: "private-unit-token".into(),
            };
            let metadata = json!({"threadId":"native-private"});
            let result = binding.resolve_subject(metadata.as_object().unwrap());
            assert_eq!(result.is_ok(), accepted);
            if accepted {
                assert_eq!(result.unwrap(), "cutex.private");
            }
            server.join().unwrap();
        }
    }
}

#[cfg(test)]
mod output_alignment_tests {
    use super::*;
    fn page(bytes: &[u8], offset: usize) -> Value {
        json!({"bytesHex":hex::encode(bytes),"fromOffset":offset,"nextOffset":offset+bytes.len(),"gap":false,"truncated":false})
    }
    #[test]
    fn tiny_byte_budgets_make_bounded_progress_without_claiming_lossless_text() {
        let bytes = "中文🦀".as_bytes();
        for budget in [1, 2] {
            let mut offset = 0;
            let mut original = Vec::new();
            while offset < bytes.len() {
                let end = (offset + budget).min(bytes.len());
                let out = readable_output(page(&bytes[offset..end], offset)).unwrap();
                let next = out["nextOffset"].as_u64().unwrap() as usize;
                assert!(next > offset && next <= end);
                assert_eq!(out["encoding"], "utf-8-lossy");
                assert_eq!(
                    out["text"],
                    String::from_utf8_lossy(&bytes[offset..next]).as_ref()
                );
                assert!(serde_json::to_vec(&out).unwrap().len() <= 12 * 1024);
                original.extend_from_slice(&bytes[offset..next]);
                offset = next;
            }
            assert_eq!(original, bytes);
        }
    }

    #[test]
    fn text_pages_preserve_unicode_and_bound_escaped_output() {
        for bytes in [
            "中文日志\n".repeat(5000).into_bytes(),
            vec![0; 20000],
            vec![255; 20000],
            vec![b'x'; 20000],
        ] {
            let mut offset = 0;
            let mut rendered = String::new();
            while offset < bytes.len() {
                let out = readable_output(page(
                    &bytes[offset..(offset + 8192).min(bytes.len())],
                    offset,
                ))
                .unwrap();
                assert!(out.get("bytesHex").is_none());
                assert!(serde_json::to_vec(&out).unwrap().len() <= 12 * 1024);
                let next = out["nextOffset"].as_u64().unwrap() as usize;
                assert!(next > offset && next <= bytes.len());
                rendered.push_str(out["text"].as_str().unwrap());
                offset = next;
            }
            assert_eq!(rendered, String::from_utf8_lossy(&bytes));
        }
    }
}
