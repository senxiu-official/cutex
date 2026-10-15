//! Fixed K ef53716 native Management semantic/receipt adapter; provider owns effects.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
const CONTRACT: &str = "cutex/agent-management/v1";
const RECEIPT_SCHEMA: &str = "cutex/agent-management-receipt/v1";
const FAILURE_SCHEMA: &str = "cutex/agent-management-failure/v1";

/// Legacy full-spec arguments (`spec` / `successor` with all ten fields,
/// `start_mode`, `frozen_message`), from before the sparse arguments (P2,
/// 2026-10-11). Accepted and validated in this release only.
// TODO(r225): remove the legacy full-spec path (ToolInput, ManagedAgentSpec,
// Operation::Create / Replace with start_mode, `is_legacy_call`) once every
// Director runs on the sparse agent_create / member_replace schema.
#[derive(Debug, Deserialize, Serialize)]
struct ToolInput {
    action_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project_id: Option<String>,
    #[serde(flatten)]
    operation: Operation,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ManagedAgentSpec {
    name: String,
    cwd: String,
    profile: String,
    runtime_backend: String,
    model: String,
    reasoning: String,
    permissions: String,
    approval_policy: String,
    sandbox_mode: String,
    groups: Vec<String>,
    #[serde(default)]
    expose_to_im: bool,
    #[serde(default)]
    pin: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    Create {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bootstrap_intent: Option<String>,
        spec: ManagedAgentSpec,
        start_mode: String,
        #[serde(default)]
        frozen_message: Option<String>,
    },
    Online {
        cutex_session_id: String,
    },
    Offline {
        cutex_session_id: String,
    },
    Restart {
        cutex_session_id: String,
    },
    Close {
        cutex_session_id: String,
    },
    Replace {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bootstrap_intent: Option<String>,
        predecessor_cutex_session_id: String,
        policy: String,
        successor: ManagedAgentSpec,
        start_mode: String,
        #[serde(default)]
        frozen_message: Option<String>,
    },
}

impl Operation {
    fn name(&self) -> &'static str {
        match self {
            Self::Create { .. } => "create",
            Self::Online { .. } => "online",
            Self::Offline { .. } => "offline",
            Self::Restart { .. } => "restart",
            Self::Close { .. } => "close",
            Self::Replace { .. } => "replace",
        }
    }
}

#[derive(Serialize)]
struct ProviderRequest<'a> {
    schema: &'static str,
    action_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_id: Option<&'a str>,
    #[serde(flatten)]
    operation: &'a Operation,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderResponse {
    schema: String,
    action_id: String,
    outcome: ProviderOutcome,
}

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum ProviderOutcome {
    Complete { receipt: Value },
    NoWrite { code: String, detail: String },
    OwnerActionRequired { failure: Value },
}

fn parse_input(arguments: &str) -> Result<ToolInput, ()> {
    let value: Value = serde_json::from_str(arguments).map_err(|_| ())?;
    let object = value.as_object().ok_or(())?;
    let operation = object.get("operation").and_then(Value::as_str).ok_or(())?;
    let operation_fields: &[&str] = match operation {
        "create" => &["spec", "start_mode", "frozen_message", "bootstrap_intent"],
        "online" | "offline" | "restart" | "close" => &["cutex_session_id"],
        "replace" => &[
            "bootstrap_intent",
            "predecessor_cutex_session_id",
            "policy",
            "successor",
            "start_mode",
            "frozen_message",
        ],
        _ => return Err(()),
    };
    if object.keys().any(|field| {
        !matches!(field.as_str(), "operation" | "action_id" | "project_id")
            && !operation_fields.contains(&field.as_str())
    }) {
        return Err(());
    }
    serde_json::from_value(value).map_err(|_| ())
}

fn request_bytes(input: &ToolInput) -> Result<(Vec<u8>, String), serde_json::Error> {
    let request = ProviderRequest {
        schema: CONTRACT,
        action_id: &input.action_id,
        project_id: input.project_id.as_deref(),
        operation: &input.operation,
    };
    let bytes = serde_json::to_vec(&request)?;
    let digest = format!("{:x}", Sha256::digest(&bytes));
    Ok((bytes, digest))
}

/// What a provider reply must be bound to: the action, project selector and
/// operation the caller sent.
struct Expect<'a> {
    action_id: &'a str,
    project_id: Option<&'a str>,
    operation: &'static str,
}

impl ToolInput {
    fn expect(&self) -> Expect<'_> {
        Expect {
            action_id: &self.action_id,
            project_id: self.project_id.as_deref(),
            operation: self.operation.name(),
        }
    }
}

fn sanitize_response(input: &Expect<'_>, digest: &str, value: Value) -> Value {
    let response: ProviderResponse = match serde_json::from_value::<ProviderResponse>(value.clone())
    {
        Ok(response) if response.schema == CONTRACT && response.action_id == input.action_id => {
            response
        }
        _ => return no_write(input.action_id, "invalid_provider_response"),
    };
    let valid = match response.outcome {
        ProviderOutcome::Complete { receipt }
            if typed_payload_matches(&receipt, RECEIPT_SCHEMA, input, Some(digest)) =>
        {
            true
        }
        ProviderOutcome::NoWrite { code, detail }
            if bounded_text(&code, 128) && bounded_text(&detail, 4096) =>
        {
            true
        }
        ProviderOutcome::OwnerActionRequired { failure }
            if typed_payload_matches(&failure, FAILURE_SCHEMA, input, None) =>
        {
            true
        }
        _ => false,
    };
    if valid {
        value
    } else {
        no_write(input.action_id, "invalid_provider_response")
    }
}

fn typed_payload_matches(
    payload: &Value,
    schema: &str,
    input: &Expect<'_>,
    digest: Option<&str>,
) -> bool {
    let Some(resolved_project) = payload.get("project_id").and_then(Value::as_str) else {
        return false;
    };
    let schema_shape_matches = match schema {
        RECEIPT_SCHEMA => receipt_shape_matches(payload, input),
        FAILURE_SCHEMA => failure_shape_matches(payload),
        _ => false,
    };
    schema_shape_matches
        && payload.get("schema").and_then(Value::as_str) == Some(schema)
        && payload.get("action_id").and_then(Value::as_str) == Some(input.action_id)
        && valid_project(resolved_project)
        && input
            .project_id
            .is_none_or(|selector| selector == resolved_project)
        && project_ids_are_consistent(payload, resolved_project)
        && payload.get("operation").and_then(Value::as_str) == Some(input.operation)
        && digest.is_none_or(|digest| {
            payload.get("request_sha256").and_then(Value::as_str) == Some(digest)
        })
}

fn receipt_shape_matches(payload: &Value, input: &Expect<'_>) -> bool {
    const FIELDS: &[&str] = &[
        "schema",
        "action_id",
        "request_sha256",
        "operation",
        "project_id",
        "completed_at",
        "result",
    ];
    let Some(object) = payload.as_object() else {
        return false;
    };
    if object.len() != FIELDS.len() || !FIELDS.iter().all(|field| object.contains_key(*field)) {
        return false;
    }
    let expected_result = match input.operation {
        "create" => "created",
        "replace" => "replaced",
        _ => "lifecycle",
    };
    object
        .get("completed_at")
        .and_then(Value::as_str)
        .is_some_and(|value| bounded_text(value, 128))
        && object
            .get("result")
            .and_then(Value::as_object)
            .and_then(|result| result.get("kind"))
            .and_then(Value::as_str)
            == Some(expected_result)
}

fn failure_shape_matches(payload: &Value) -> bool {
    const REQUIRED_FIELDS: &[&str] = &[
        "schema",
        "event_id",
        "action_id",
        "project_id",
        "operation",
        "code",
        "detail",
        "routing_status",
        "created_at",
    ];
    const OPTIONAL_FIELDS: &[&str] = &["route_to_director_session", "target_cutex_session_id"];
    let Some(object) = payload.as_object() else {
        return false;
    };
    if !(REQUIRED_FIELDS.len()..=REQUIRED_FIELDS.len() + OPTIONAL_FIELDS.len())
        .contains(&object.len())
        || !REQUIRED_FIELDS
            .iter()
            .all(|field| object.contains_key(*field))
        || object.keys().any(|field| {
            !REQUIRED_FIELDS.contains(&field.as_str()) && !OPTIONAL_FIELDS.contains(&field.as_str())
        })
    {
        return false;
    }
    object
        .get("event_id")
        .and_then(Value::as_str)
        .is_some_and(valid_identity)
        && object
            .get("code")
            .and_then(Value::as_str)
            .is_some_and(|value| bounded_text(value, 128))
        && object
            .get("detail")
            .and_then(Value::as_str)
            .is_some_and(|value| value.len() <= 4096)
        && matches!(
            object.get("routing_status").and_then(Value::as_str),
            Some("routable" | "unrouted")
        )
        && object
            .get("created_at")
            .and_then(Value::as_str)
            .is_some_and(|value| bounded_text(value, 128))
        && OPTIONAL_FIELDS.iter().all(|field| {
            object.get(*field).is_none_or(|value| {
                value.is_null()
                    || value
                        .as_str()
                        .is_some_and(|value| valid_identity(value) && value.len() <= 256)
            })
        })
}

fn project_ids_are_consistent(value: &Value, expected: &str) -> bool {
    match value {
        // Imported/moved roster records retain historical creation provenance:
        // project_id may be null or an old Project. It is NOT current membership.
        // Recognize the exact provider record type rather than weakening all
        // nested project checks; current authority/grants still bind the envelope.
        Value::Object(object)
            if object.contains_key("cutex_session_id")
                && serde_json::from_value::<crate::agent_management::ManagedAgentRecord>(
                    value.clone(),
                )
                .is_ok() =>
        {
            true
        }
        Value::Object(object) => object.iter().all(|(key, value)| {
            if key == "project_id" {
                value.as_str() == Some(expected)
            } else {
                project_ids_are_consistent(value, expected)
            }
        }),
        Value::Array(values) => values
            .iter()
            .all(|value| project_ids_are_consistent(value, expected)),
        _ => true,
    }
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/' | b'@')
        })
}

fn valid_project(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b':'))
}

fn bounded_text(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max
}

fn no_write(action_id: &str, code: &str) -> Value {
    serde_json::json!({
        "schema": CONTRACT,
        "action_id": action_id,
        "outcome": {
            "status": "no_write",
            "code": code,
            "detail": "No successful action receipt is available. This is not proof of rollback; an unconfirmed provider outcome requires exact-action retry."
        }
    })
}

/// The arguments were refused here, before anything was sent: a corrected
/// call (any action_id) is fine. Not the "exact-action retry" wording of
/// [`no_write`], which is for unconfirmed provider outcomes.
fn invalid(action_id: &str, code: &str, reason: &str) -> Value {
    json!({
        "schema": CONTRACT,
        "action_id": action_id,
        "outcome": {
            "status": "no_write",
            "code": code,
            "detail": format!("{reason} Nothing was sent; correct the arguments and call again.")
        }
    })
}

fn uncertain(action_id: &str) -> Value {
    let mut failure = no_write(action_id, "response_uncertain");
    failure["outcome"]["detail"] = json!(
        "Outcome unknown; retry the exact action_id and payload, never a replacement action."
    );
    failure
}

/// Fields only the legacy full-spec arguments have.
const LEGACY_FIELDS: &[&str] = &["spec", "start_mode", "frozen_message"];

/// Agent fields of the sparse arguments (agent_create top level,
/// member_replace `successor`).
const AGENT_FIELDS: &[&str] = &[
    "name",
    "profile",
    "model",
    "reasoning",
    "sandbox",
    "approval_policy",
    "cwd",
    // Retired (`im-exposure-retired`): no longer in the tool schema, still
    // accepted from older prompts and ignored (forwarded only so an exact
    // replay keeps its digest).
    "expose_to_im",
    "pin",
];

/// Legacy full-spec fields that the sparse arguments dropped, with what to
/// do instead.
fn dropped_field_hint(field: &str) -> Option<&'static str> {
    Some(match field {
        "runtime_backend" => "runtime_backend was removed: it follows from the profile.",
        "permissions" | "sandbox_mode" => "permissions / sandbox_mode were removed: use sandbox (or omit it to inherit yours).",
        "groups" => "groups was removed: the new agent joins your Agent Bus groups.",
        _ => return None,
    })
}

// TODO(r225): drop with the legacy full-spec path.
fn is_legacy_call(args: &Value) -> bool {
    matches!(args["operation"].as_str(), Some("create" | "replace"))
        && LEGACY_FIELDS.iter().any(|field| args.get(*field).is_some())
}

pub(super) fn invoke(args: Value, post: impl FnMut(&Value) -> anyhow::Result<Value>) -> Value {
    match args["operation"].as_str() {
        Some("create" | "replace") if !is_legacy_call(&args) => invoke_sparse(args, post),
        _ => invoke_full(args, post),
    }
}

fn transport(
    expect: &Expect<'_>,
    digest: &str,
    body: &Value,
    mut post: impl FnMut(&Value) -> anyhow::Result<Value>,
) -> Value {
    match post(body) {
        Ok(value)
            if value["http_status"]
                .as_u64()
                .is_some_and(|s| s >= 500 || s == 408) =>
        {
            uncertain(expect.action_id)
        }
        Ok(value) => sanitize_response(expect, digest, value),
        Err(_) => uncertain(expect.action_id),
    }
}

/// Sparse agent_create / member_replace: the Agent Bus fills every omitted
/// field from the caller and its project (`agent_management::sparse`).
fn invoke_sparse(args: Value, post: impl FnMut(&Value) -> anyhow::Result<Value>) -> Value {
    let Some(object) = args.as_object() else {
        return invalid("invalid-body", "invalid_arguments", "Arguments must be an object.");
    };
    let operation = if object["operation"] == "create" { "create" } else { "replace" };
    let action_id = match object.get("action_id") {
        None | Some(Value::Null) => return invalid("invalid-body", "missing_action_id", "action_id is missing."),
        Some(Value::String(id)) if valid_identity(id) => id.clone(),
        Some(_) => return invalid("invalid-body", "invalid_arguments", "action_id must be a short id of letters, digits and - _ . : / @."),
    };
    let refuse = |code: &str, reason: String| invalid(&action_id, code, &reason);
    let top: &[&str] = if operation == "create" {
        &["task", "task_mode", "bootstrap_intent"]
    } else {
        &["predecessor_cutex_session_id", "policy", "successor", "task", "task_mode", "bootstrap_intent"]
    };
    let unknown = |field: &str, allowed: &[&str]| {
        let hint = dropped_field_hint(field)
            .map(str::to_string)
            .unwrap_or_else(|| format!("Unknown field `{field}`; allowed: {}.", allowed.join(", ")));
        refuse("invalid_arguments", hint)
    };
    let mut allowed_top: Vec<&str> = vec!["action_id", "project_id"];
    allowed_top.extend_from_slice(top);
    if operation == "create" {
        allowed_top.extend_from_slice(AGENT_FIELDS);
    }
    for field in object.keys().filter(|field| field.as_str() != "operation") {
        if !allowed_top.contains(&field.as_str()) {
            return unknown(field, &allowed_top);
        }
    }
    let agent = if operation == "create" {
        Value::Object(
            object
                .iter()
                .filter(|(field, value)| AGENT_FIELDS.contains(&field.as_str()) && !value.is_null())
                .map(|(field, value)| (field.clone(), value.clone()))
                .collect(),
        )
    } else {
        match object.get("successor") {
            Some(Value::Object(successor)) => {
                if let Some(field) = successor.keys().find(|field| !AGENT_FIELDS.contains(&field.as_str())) {
                    return unknown(field, AGENT_FIELDS);
                }
                Value::Object(
                    successor
                        .iter()
                        .filter(|(_, value)| !value.is_null())
                        .map(|(field, value)| (field.clone(), value.clone()))
                        .collect(),
                )
            }
            None | Some(Value::Null) => return refuse("missing_successor", "successor is missing.".into()),
            Some(_) => return refuse("invalid_arguments", "successor must be an object with at least name.".into()),
        }
    };
    if agent.get("name").and_then(Value::as_str).is_none_or(|name| name.trim().is_empty()) {
        let field = if operation == "create" { "name" } else { "successor.name" };
        return refuse("missing_name", format!("{field} is missing."));
    }
    for (field, missing) in [("predecessor_cutex_session_id", "missing_predecessor_cutex_session_id"), ("policy", "missing_policy")] {
        if operation == "replace" && object.get(field).is_none_or(Value::is_null) {
            return refuse(missing, format!("{field} is missing."));
        }
    }
    let mut body = json!({
        "schema": crate::agent_management::SPARSE_SCHEMA,
        "action_id": action_id,
        "operation": operation,
    });
    for field in ["project_id", "task", "task_mode", "bootstrap_intent", "predecessor_cutex_session_id", "policy"] {
        if let Some(value) = object.get(field).filter(|value| !value.is_null()) {
            body[field] = value.clone();
        }
    }
    body[if operation == "create" { "agent" } else { "successor" }] = agent;
    if body["project_id"].as_str().is_some_and(|p| !valid_project(p)) {
        return refuse("invalid_arguments", "project_id is not a valid project id.".into());
    }
    // The typed request is the semantic digest (not the model's key order).
    let request: crate::agent_management::SparseAgentManagementRequest =
        match serde_json::from_value(body) {
            Ok(request) => request,
            Err(error) => return refuse("invalid_arguments", format!("Invalid arguments: {error}.")),
        };
    let Ok(digest) = request.digest() else {
        return refuse("invalid_arguments", "Arguments are not serializable.".into());
    };
    let expect = Expect {
        action_id: &action_id,
        project_id: request.project_id.as_ref().map(|project| project.as_str()),
        operation,
    };
    transport(&expect, digest.as_str(), &json!(request), post)
}

fn invoke_full(args: Value, post: impl FnMut(&Value) -> anyhow::Result<Value>) -> Value {
    let required: &[&str] = match args["operation"].as_str() {
        Some("create") => &["spec", "start_mode"],
        Some("online" | "offline" | "restart" | "close") => &["cutex_session_id"],
        Some("replace") => &[
            "predecessor_cutex_session_id",
            "policy",
            "successor",
            "start_mode",
        ],
        _ => &[],
    };
    if let Some(field) = required
        .iter()
        .find(|field| args.get(**field).is_none_or(Value::is_null))
    {
        return invalid("invalid-body", &format!("missing_{field}"), &format!("{field} is missing."));
    }
    if args.get("action_id").is_none_or(Value::is_null) {
        return invalid("invalid-body", "missing_action_id", "action_id is missing.");
    }
    let input = match parse_input(&args.to_string()) {
        Ok(input) => input,
        Err(_) => return invalid("invalid-body", "invalid_arguments", "The arguments do not match the tool's schema."),
    };
    if !valid_identity(&input.action_id)
        || input
            .project_id
            .as_deref()
            .is_some_and(|p| !valid_project(p))
    {
        return invalid("invalid-body", "invalid_arguments", "action_id or project_id is not a valid id.");
    }
    let invalid_input = || invalid(&input.action_id, "invalid_arguments", "The arguments do not match the tool's schema.");
    let (bytes, _) = match request_bytes(&input) {
        Ok(value) => value,
        Err(_) => return invalid_input(),
    };
    // Serialize the current provider type for its exact semantic digest, rather
    // than relying on JSON object key order or a model-supplied hash.
    let request: crate::agent_management::AgentManagementRequest =
        match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(_) => return invalid_input(),
        };
    let digest = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&request).expect("typed request"))
    );
    transport(&input.expect(), &digest, &json!(request), post)
}

pub(super) const AGENT_CREATE: &str = "agent_create";
pub(super) const AGENT_ARCHIVE: &str = "agent_archive";
pub(super) const MEMBER_RUNTIME: &str = "member_runtime";
pub(super) const MEMBER_REPLACE: &str = "member_replace";

pub(super) fn is_tool(name: &str) -> bool {
    matches!(
        name,
        AGENT_CREATE | AGENT_ARCHIVE | MEMBER_RUNTIME | MEMBER_REPLACE
    )
}

/// Arguments of a Management tool in the provider's operation shape
/// (`operation` + its fields), which [`invoke`] validates; fields the tool
/// does not take make `invoke` refuse with `invalid_arguments`.
pub(super) fn operation_args(tool: &str, args: Value) -> Value {
    let mut args = if args.is_null() { json!({}) } else { args };
    let Some(object) = args.as_object_mut() else {
        return args;
    };
    if object.contains_key("operation") {
        // `operation` is implied by the tool; an explicit one is invalid.
        object.insert("operation".into(), json!("invalid"));
        return args;
    }
    let operation = match tool {
        AGENT_CREATE => "create",
        AGENT_ARCHIVE => "close",
        MEMBER_REPLACE => "replace",
        MEMBER_RUNTIME => match object.remove("change") {
            Some(Value::String(change)) if matches!(change.as_str(), "online" | "offline" | "restart") => {
                object.insert("operation".into(), json!(change));
                return args;
            }
            _ => "invalid",
        },
        _ => "invalid",
    };
    object.insert("operation".into(), json!(operation));
    args
}

/// The old one-tool shape (`cutex_agent_management` with `operation`) as a
/// new tool call; `None` for `query_managed` (replaced by `agent_list`) and
/// operations agents never had.
pub(super) fn legacy_call(args: &Value) -> Option<(&'static str, Value)> {
    let mut args = args.clone();
    let object = args.as_object_mut()?;
    let operation = object.remove("operation")?;
    let tool = match operation.as_str()? {
        "create" => AGENT_CREATE,
        "close" => AGENT_ARCHIVE,
        "replace" => MEMBER_REPLACE,
        change @ ("online" | "offline" | "restart") => {
            object.insert("change".into(), json!(change));
            MEMBER_RUNTIME
        }
        _ => return None,
    };
    Some((tool, args))
}

/// The optional agent fields of agent_create (top level) and of
/// member_replace's `successor`; each defaults when omitted.
fn agent_fields(name: &str) -> serde_json::Map<String, Value> {
    let string = |description: &str| json!({"type":"string","description":description});
    let mut fields = serde_json::Map::new();
    fields.insert("name".into(), string(name));
    fields.insert("profile".into(), string("Profile (name or id) it runs on. Default: yours."));
    fields.insert("model".into(), string("Model. Default: the profile's default model."));
    fields.insert("reasoning".into(), string("Reasoning effort. Default: the profile's default. An effort the model does not support is refused with the allowed list."));
    fields.insert("sandbox".into(), json!({"type":"string","enum":["read-only","workspace-write","danger-full-access"],"description":"Default: yours; never more than yours."}));
    fields.insert("approval_policy".into(), json!({"type":"string","enum":["untrusted","on-request","never"],"description":"Default: yours; never less strict than yours."}));
    fields.insert("cwd".into(), string("Absolute working directory. Default: the project's workspace root (else yours); agents may share it."));
    fields.insert("pin".into(), json!({"type":"boolean","description":"Pin it in the owner's agent list. Default false."}));
    fields
}

pub(super) fn tools() -> Vec<Value> {
    let string = |description: &str| json!({"type":"string","description":description});
    let action_id = string("Optional; derived from the call when omitted. Pass one only to repeat an exact earlier call (exact replay).");
    let project_id = string("Optional: your project's id; needed only when you manage more than one project.");
    let target = |verb: &str| string(&format!("ID or exact name of the member to {verb}; IDs accept a unique prefix or suffix of at least 6 hex characters."));
    let task = string("Optional brief: what to do and what to return. It is delegated to the new agent from you, as with agent_delegate: its questions and progress come to you and its result (the final message of the turn that handles it) comes back to you automatically, without polling. The result's delegation gives the delegation id and state for the delegation_* tools; if it says ok=false the agent exists but never got the task. Omit it to start the agent idle.");
    let task_mode = json!({"type":"string","enum":["delegation","message"],"description":"How task is handed over. delegation (default): a tracked delegation from you. message: a plain first message with no tracked result; the result's first_message gives its delivery state (delivered; queued; quarantined or failed: never delivered)."});
    let bootstrap_intent = string("Optional: action_id of a bundle intent the owner already reviewed. Never mints an intent or grants authority.");
    let name = "Name of the new agent; unique among active agents on this host (a taken name is refused with a free suggestion).";
    let tool = |name: &str, description: String, properties: Value, required: &[&str]| {
        json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false}})
    };
    const AUTHORITY: &str = "Your authority comes from your runtime, never from arguments. After response_uncertain, repeat the identical call (pass the action_id from the reply in a later turn).";
    const LIMITS: &str = "A new agent never gets more than you have: a sandbox, approval policy, network access or writable roots beyond yours answers permission_exceeds_creator with the offending fields. A profile the owner marked as needing approval answers approval_required (approval_id): repeat the same call with the same action_id after the owner approves; do not poll.";
    const DEFAULTS: &str = "Leave the other fields empty unless needed: profile, model, reasoning, sandbox and approval policy default to yours, cwd to the project's workspace.";
    let mut create = agent_fields(name);
    create.insert("task".into(), task.clone());
    create.insert("task_mode".into(), task_mode.clone());
    for (field, schema) in [("action_id", &action_id), ("project_id", &project_id), ("bootstrap_intent", &bootstrap_intent)] {
        create.insert(field.into(), schema.clone());
    }
    let successor = json!({"type":"object","properties":agent_fields("Name of the successor agent (it may reuse the old name with close_before_create)."),
        "required":["name"],"additionalProperties":false,
        "description":"The new agent: name, plus any field to change from your defaults (as in agent_create)."});
    vec![
        tool(AGENT_CREATE, format!("Example: {{\"name\":\"quokka\",\"task\":\"Review src/parser and report bugs\"}}. {DEFAULTS} (Director or operator) Create and start a persistent cutex agent in your project. It has its own conversation the owner can use. The profile, model and reasoning fields select its runtime configuration; omitted fields use the documented defaults. {LIMITS} {AUTHORITY}"),
            Value::Object(create),
            &["name"]),
        tool(AGENT_ARCHIVE, format!("(Director or operator) Archive a member of your project: it stops and is retired for good (agents cannot undo this). {AUTHORITY}"),
            json!({"action_id":action_id,"project_id":project_id,"cutex_session_id":target("archive")}),
            &["cutex_session_id"]),
        tool(MEMBER_RUNTIME, format!("(Director or operator) Bring a member of your project online, take it offline, or restart it; cutex resolves its launch contract (profile, model, permissions). {AUTHORITY}"),
            json!({"action_id":action_id,"project_id":project_id,"cutex_session_id":target("change"),"change":{"type":"string","enum":["online","offline","restart"]}}),
            &["cutex_session_id","change"]),
        tool(MEMBER_REPLACE, format!("Example: {{\"predecessor_cutex_session_id\":\"quokka\",\"policy\":\"close_after_ready\",\"successor\":{{\"name\":\"quokka-2\"}},\"task\":\"Continue the parser review\"}}. {DEFAULTS} (Director or operator) Replace a member of your project with a new agent (successor). policy: close_before_create, close_after_ready (the old one is archived once the new one is ready) or keep_old. {LIMITS} {AUTHORITY}"),
            json!({"action_id":action_id,"project_id":project_id,"predecessor_cutex_session_id":target("replace"),
                "policy":{"type":"string","enum":["close_before_create","close_after_ready","keep_old"]},
                "successor":successor,"task":task,"task_mode":task_mode,"bootstrap_intent":bootstrap_intent}),
            &["predecessor_cutex_session_id","policy","successor"]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Value {
        json!({"name":"Formal Agent","cwd":"/private","profile":"mutable",
            "runtime_backend":"host","model":"fixture","reasoning":"low",
            "permissions":"read-only","approval_policy":"on-request",
            "sandbox_mode":"read-only","groups":["private"]})
    }

    fn call(tool: &str, args: Value, post: impl FnMut(&Value) -> anyhow::Result<Value>) -> Value {
        invoke(operation_args(tool, args), post)
    }

    #[test]
    fn every_tool_preserves_typed_request_digest_and_receipt() {
        for (tool, fields, operation, kind) in [
            (
                AGENT_CREATE,
                json!({"spec":spec(),"start_mode":"bootstrap_only"}),
                "create",
                "created",
            ),
            (MEMBER_RUNTIME, json!({"cutex_session_id":"cutex.target","change":"online"}), "online", "lifecycle"),
            (MEMBER_RUNTIME, json!({"cutex_session_id":"cutex.target","change":"offline"}), "offline", "lifecycle"),
            (MEMBER_RUNTIME, json!({"cutex_session_id":"cutex.target","change":"restart"}), "restart", "lifecycle"),
            (AGENT_ARCHIVE, json!({"cutex_session_id":"cutex.target"}), "close", "lifecycle"),
            (
                MEMBER_REPLACE,
                json!({"predecessor_cutex_session_id":"cutex.target","policy":"keep_old","successor":spec(),"start_mode":"custom_message","frozen_message":"exact 私有\n"}),
                "replace",
                "replaced",
            ),
        ] {
            let mut args = json!({"action_id":"same-action","project_id":"private"});
            args.as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            let mut expected = Value::Null;
            let actual = call(tool, args, |request| {
                let typed: crate::agent_management::AgentManagementRequest =
                    serde_json::from_value(request.clone())?;
                assert_eq!(request["operation"], operation);
                assert_eq!(request["project_id"], "private");
                assert!(request.get("change").is_none());
                expected = json!({"schema":CONTRACT,"action_id":"same-action","outcome":{"status":"complete","receipt":{
                    "schema":RECEIPT_SCHEMA,"action_id":"same-action","project_id":"private","operation":operation,
                    "request_sha256":format!("{:x}",Sha256::digest(serde_json::to_vec(&typed)?)),
                    "completed_at":"immutable-time","result":{"kind":kind,"historical_snapshot":"unchanged"}}}});
                Ok(expected.clone())
            });
            assert_eq!(actual, expected, "{tool} {operation}");
        }
    }

    #[test]
    fn retired_expose_to_im_is_accepted_but_not_advertised() {
        for tool in tools() {
            let properties = &tool["inputSchema"]["properties"];
            assert!(properties.get("expose_to_im").is_none(), "{}", tool["name"]);
            if let Some(successor) = properties.get("successor") {
                assert!(successor["properties"].get("expose_to_im").is_none());
            }
        }
        let mut transported = 0;
        let result = call(
            AGENT_CREATE,
            json!({"action_id":"legacy-im","name":"worker","expose_to_im":false}),
            |request| {
                transported += 1;
                let typed: crate::agent_management::SparseAgentManagementRequest =
                    serde_json::from_value(request.clone())?;
                drop(typed);
                Ok(json!({"http_status":200}))
            },
        );
        assert_eq!(transported, 1, "{result}");
        assert_ne!(result["outcome"]["code"], "invalid_arguments", "{result}");
    }

    #[test]
    fn short_id_management_receipts_use_canonical_request_digest() {
        crate::asks::short_ids::tests::context("short-management", |ctx| {
            let mut requests = Vec::new();
            for id in ["KiMi", "cutex.…0000b1", crate::asks::store::tests::TARGET] {
                let args = crate::asks::short_ids::normalize(ctx, MEMBER_RUNTIME,
                    json!({"action_id":"same-action","project_id":"private","cutex_session_id":id,"change":"offline"})).unwrap();
                let result = call(MEMBER_RUNTIME, args, |request| {
                    let typed: crate::agent_management::AgentManagementRequest = serde_json::from_value(request.clone())?;
                    requests.push(request.clone());
                    Ok(json!({"schema":CONTRACT,"action_id":"same-action","outcome":{"status":"complete","receipt":{
                        "schema":RECEIPT_SCHEMA,"action_id":"same-action","project_id":"private","operation":"offline",
                        "request_sha256":format!("{:x}",Sha256::digest(serde_json::to_vec(&typed)?)),
                        "completed_at":"immutable-time","result":{"kind":"lifecycle"}}}}))
                });
                assert_eq!(result["outcome"]["status"], "complete", "{result}");
            }
            assert!(requests.windows(2).all(|pair| pair[0]==pair[1]));
        });
    }

    #[test]
    fn invalid_or_authority_fields_never_reach_transport() {
        for extra in [
            json!({"caller_cutex_session_id":"spoof"}),
            json!({"token":"secret"}),
            json!({"generation":1}),
            json!({"spec":spec()}),
            json!({"change":"online"}),
            json!({"operation":"close"}),
            json!({"operation":"grant_operator"}),
            json!({"operation":"query_managed"}),
        ] {
            let mut args = json!({"action_id":"check","cutex_session_id":"cutex.target"});
            args.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert_eq!(
                call(AGENT_ARCHIVE, args, |_| panic!("invalid input transported"))["outcome"]["code"],
                "invalid_arguments",
                "{extra}"
            );
        }
        for change in [json!("activate"), json!(null), json!(1)] {
            assert_eq!(
                call(MEMBER_RUNTIME, json!({"action_id":"check","cutex_session_id":"cutex.target","change":change}),
                    |_| panic!("invalid change transported"))["outcome"]["code"],
                "invalid_arguments"
            );
        }
        for (tool, args, field) in [
            (MEMBER_RUNTIME, json!({"action_id":"check","change":"online"}), "cutex_session_id"),
            (AGENT_ARCHIVE, json!({"action_id":"check"}), "cutex_session_id"),
            (AGENT_CREATE, json!({"action_id":"check"}), "name"),
            (AGENT_CREATE, json!({"action_id":"check","start_mode":"bootstrap_only"}), "spec"),
            (MEMBER_REPLACE, json!({"action_id":"check","policy":"keep_old","successor":{"name":"n"}}), "predecessor_cutex_session_id"),
            (MEMBER_REPLACE, json!({"action_id":"check","predecessor_cutex_session_id":"cutex.old","policy":"keep_old"}), "successor"),
            (MEMBER_REPLACE, json!({"action_id":"check","start_mode":"bootstrap_only"}), "predecessor_cutex_session_id"),
        ] {
            assert_eq!(
                call(tool, args, |_| panic!("missing field transported"))["outcome"]["code"],
                format!("missing_{field}")
            );
        }
        let mut bad = spec();
        bad["principal"] = json!("root");
        assert_eq!(
            call(
                AGENT_CREATE,
                json!({"action_id":"check","spec":bad,"start_mode":"bootstrap_only"}),
                |_| panic!("nested control transported")
            )["outcome"]["code"],
            "invalid_arguments"
        );
        assert_eq!(
            call("cutex_agent_management", json!({"action_id":"check"}), |_| panic!("unknown tool transported"))["outcome"]["code"],
            "invalid_arguments"
        );
        // r219: director_rotate is no longer an operation of this adapter.
        assert_eq!(
            invoke(
                json!({"operation":"director_rotate","action_id":"check","expected_predecessor_cutex_session":"cutex.old",
                    "expected_authority_epoch":1,"mode":"retain_predecessor_bootstrap_only","successor":spec()}),
                |_| panic!("removed operation transported")
            )["outcome"]["code"],
            "invalid_arguments"
        );
    }

    #[test]
    fn reviewed_references_preserve_exact_semantics_without_minting_authority() {
        for (tool, operation, fields) in [
            (
                AGENT_CREATE,
                "create",
                json!({"spec":spec(),"start_mode":"bootstrap_only"}),
            ),
            (
                MEMBER_REPLACE,
                "replace",
                json!({"predecessor_cutex_session_id":"cutex.old","policy":"keep_old","successor":spec(),"start_mode":"bootstrap_only"}),
            ),
        ] {
            let mut args = json!({"action_id":"reviewed","project_id":"private","bootstrap_intent":"reviewed"});
            args.as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            let result = call(tool, args.clone(), |body| {
                assert_eq!(body["bootstrap_intent"], "reviewed");
                assert_eq!(body["operation"], operation);
                Ok(
                    json!({"schema":CONTRACT,"action_id":"reviewed","outcome":{"status":"no_write","code":"bootstrap_intent_missing","detail":"no Human intent"}}),
                )
            });
            assert_eq!(result["outcome"]["code"], "bootstrap_intent_missing");
            args["bootstrap_intent"] = json!("different");
            // Provider validation is also mandatory, before any lifecycle effect.
            let mut request = operation_args(tool, args);
            request["schema"] = json!(CONTRACT);
            let request: crate::agent_management::AgentManagementRequest =
                serde_json::from_value(request).unwrap();
            assert!(request.validate().is_err());
        }
    }

    #[test]
    fn uncertainty_and_provider_denial_are_not_success_and_receipts_are_bound() {
        let args = json!({"action_id":"check","project_id":"private","cutex_session_id":"cutex.target"});
        let uncertain = call(AGENT_ARCHIVE, args.clone(), |_| anyhow::bail!("secret transport detail"));
        assert_eq!(uncertain["outcome"]["code"], "response_uncertain");
        assert!(!uncertain.to_string().contains("secret"));
        for status in [408, 500, 503] {
            let value = call(AGENT_ARCHIVE, args.clone(), |_| {
                Ok(json!({"http_status":status,"error":"private transport detail"}))
            });
            assert_eq!(value["outcome"]["code"], "response_uncertain");
            assert!(!value.to_string().contains("private transport detail"));
        }
        let denied = json!({"schema":CONTRACT,"action_id":"check","outcome":{"status":"no_write","code":"conflict","detail":"exact semantic conflict"}});
        assert_eq!(call(AGENT_ARCHIVE, args.clone(), |_| Ok(denied.clone())), denied);
        let failure = json!({"schema":CONTRACT,"action_id":"check","outcome":{"status":"owner_action_required","failure":{
            "schema":FAILURE_SCHEMA,"event_id":"event-1","action_id":"check","project_id":"private","operation":"close",
            "code":"owner_action_required","detail":"retained stage","routing_status":"unrouted","created_at":"time"}}});
        assert_eq!(call(AGENT_ARCHIVE, args.clone(), |_| Ok(failure.clone())), failure);
        for field in ["project_id", "operation", "action_id"] {
            let mut wrong = failure.clone();
            wrong["outcome"]["failure"][field] = json!("foreign");
            assert_eq!(
                call(AGENT_ARCHIVE, args.clone(), |_| Ok(wrong.clone()))["outcome"]["code"],
                "invalid_provider_response"
            );
        }
    }

    #[test]
    fn tool_schemas_are_focused_and_closed() {
        let tools = tools();
        let names: Vec<_> = tools.iter().map(|tool| tool["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [AGENT_CREATE, AGENT_ARCHIVE, MEMBER_RUNTIME, MEMBER_REPLACE]
        );
        for tool in &tools {
            let schema = &tool["inputSchema"];
            assert!(is_tool(tool["name"].as_str().unwrap()));
            assert_eq!(schema["additionalProperties"], false);
            assert!(schema["properties"].get("operation").is_none());
            // action_id is optional: the facade derives it per call.
            assert!(schema["properties"].get("action_id").is_some());
            assert!(!schema["required"].as_array().unwrap().contains(&json!("action_id")));
            for spec in ["spec", "successor"] {
                if let Some(spec) = schema["properties"].get(spec) {
                    assert_eq!(spec["additionalProperties"], false);
                }
            }
            for required in schema["required"].as_array().unwrap() {
                assert!(schema["properties"].get(required.as_str().unwrap()).is_some());
            }
        }
        // P2: sparse create / replace; every field explained; no legacy fields.
        let create = &tools[0];
        assert_eq!(create["inputSchema"]["required"], json!(["name"]));
        let description = create["description"].as_str().unwrap();
        assert!(description.starts_with("Example: {\"name\":\"quokka\",\"task\":"), "{description}");
        assert!(description.contains("Leave the other fields empty unless needed"), "{description}");
        // Prompt revision A2: what it creates, not which work it is for.
        assert!(description.contains("Create and start a persistent cutex agent in your project. It has its own conversation the owner can use."), "{description}");
        for preference in ["long-lived work", "prefer your native", "short parallel subtasks"] {
            assert!(!description.contains(preference), "{preference}");
        }
        assert!(description.contains("permission_exceeds_creator") && description.contains("approval_required") && description.contains("response_uncertain"), "limits and replay kept");
        let replace = &tools[3];
        assert!(replace["description"].as_str().unwrap().starts_with("Example: {"));
        for (tool, properties) in [
            (AGENT_CREATE, &create["inputSchema"]["properties"]),
            (MEMBER_REPLACE, &replace["inputSchema"]["properties"]["successor"]["properties"]),
        ] {
            for (field, schema) in properties.as_object().unwrap() {
                assert!(schema["description"].as_str().is_some_and(|d| !d.is_empty()), "{tool}.{field}");
            }
            for legacy in ["spec", "start_mode", "frozen_message", "runtime_backend", "permissions", "sandbox_mode", "groups"] {
                assert!(properties.get(legacy).is_none(), "{tool}.{legacy}");
            }
        }
        assert_eq!(create["inputSchema"]["properties"]["sandbox"]["enum"], json!(["read-only","workspace-write","danger-full-access"]));
        assert_eq!(create["inputSchema"]["properties"]["approval_policy"]["enum"], json!(["untrusted","on-request","never"]));
    }

    fn sparse_receipt(request: &Value, operation: &str, kind: &str) -> anyhow::Result<Value> {
        let typed: crate::agent_management::SparseAgentManagementRequest = serde_json::from_value(request.clone())?;
        Ok(json!({"schema":CONTRACT,"action_id":typed.action_id.as_str(),"outcome":{"status":"complete","receipt":{
            "schema":RECEIPT_SCHEMA,"action_id":typed.action_id.as_str(),"project_id":"private","operation":operation,
            "request_sha256":typed.digest()?.as_str(),
            "completed_at":"immutable-time","result":{"kind":kind}}}}))
    }

    #[test]
    fn sparse_create_and_replace_send_only_what_was_given_bound_to_their_digest() {
        let mut sent = Vec::new();
        let created = call(AGENT_CREATE, json!({"action_id":"sparse-1","name":"quokka","task":"map the repo","sandbox":"read-only","model":null}), |body| {
            assert_eq!(body["schema"], crate::agent_management::SPARSE_SCHEMA);
            assert_eq!(body["operation"], "create");
            assert_eq!(body["agent"], json!({"name":"quokka","sandbox":"read-only"}));
            assert_eq!(body["task"], "map the repo");
            sent.push(body.clone());
            sparse_receipt(body, "create", "created")
        });
        assert_eq!(created["outcome"]["status"], "complete", "{created}");
        // Key order and explicit defaults (pin=false) do not change the digest.
        let again = call(AGENT_CREATE, json!({"task":"map the repo","pin":false,"sandbox":"read-only","name":"quokka","action_id":"sparse-1"}), |body| {
            sent.push(body.clone());
            sparse_receipt(body, "create", "created")
        });
        assert_eq!(again, created);
        let digests: Vec<_> = sent.iter().map(|body| serde_json::from_value::<crate::agent_management::SparseAgentManagementRequest>(body.clone()).unwrap().digest().unwrap()).collect();
        assert_eq!(digests[0], digests[1]);

        let replaced = call(MEMBER_REPLACE, json!({"action_id":"sparse-2","predecessor_cutex_session_id":"cutex.old","policy":"close_after_ready","successor":{"name":"quokka-2"}}), |body| {
            assert_eq!(body["operation"], "replace");
            assert_eq!(body["successor"], json!({"name":"quokka-2"}));
            assert!(body.get("task").is_none());
            sparse_receipt(body, "replace", "replaced")
        });
        assert_eq!(replaced["outcome"]["status"], "complete", "{replaced}");

        // A receipt for another request (the full one's digest) is refused.
        let foreign = call(AGENT_CREATE, json!({"action_id":"sparse-3","name":"emu"}), |body| {
            let mut receipt = sparse_receipt(body, "create", "created")?;
            receipt["outcome"]["receipt"]["request_sha256"] = json!("0".repeat(64));
            Ok(receipt)
        });
        assert_eq!(foreign["outcome"]["code"], "invalid_provider_response");
    }

    /// agent-create-task-delegation: task_mode is forwarded (and bound to
    /// the digest); a delegation reply inside the receipt's result passes
    /// the receipt checks; an unknown mode never reaches the transport.
    #[test]
    fn task_mode_is_forwarded_and_the_delegation_reply_passes() {
        let created = call(AGENT_CREATE, json!({"action_id":"tm-1","name":"quokka","task":"map","task_mode":"message"}), |body| {
            assert_eq!(body["task_mode"], "message");
            sparse_receipt(body, "create", "created")
        });
        assert_eq!(created["outcome"]["status"], "complete", "{created}");
        let replaced = call(MEMBER_REPLACE, json!({"action_id":"tm-2","predecessor_cutex_session_id":"cutex.old",
            "policy":"keep_old","successor":{"name":"q-2"},"task":"go on","task_mode":"delegation"}), |body| {
            assert_eq!(body["task_mode"], "delegation");
            sparse_receipt(body, "replace", "replaced")
        });
        assert_eq!(replaced["outcome"]["status"], "complete", "{replaced}");
        let delegated = call(AGENT_CREATE, json!({"action_id":"tm-3","name":"emu","task":"map"}), |body| {
            assert!(body.get("task_mode").is_none(), "default not sent: {body}");
            let mut receipt = sparse_receipt(body, "create", "created")?;
            receipt["outcome"]["receipt"]["result"]["task_mode"] = json!("delegation");
            receipt["outcome"]["receipt"]["result"]["delegation"] = json!({"ok":true,"delegation":"delegation-0123abcd",
                "state":"queued","created":true,"target_id":"cutex.new","detail":"Queued for emu."});
            Ok(receipt)
        });
        assert_eq!(delegated["outcome"]["receipt"]["result"]["delegation"]["delegation"], "delegation-0123abcd", "{delegated}");
        let refused = call(AGENT_CREATE, json!({"action_id":"tm-4","name":"emu","task":"map","task_mode":"note"}), |_| {
            panic!("an unknown task_mode must not reach the transport")
        });
        assert_eq!(refused["outcome"]["code"], "invalid_arguments", "{refused}");
        assert!(tools()[0]["inputSchema"]["properties"]["task"]["description"].as_str().unwrap().contains("delegated"));
        assert_eq!(tools()[0]["inputSchema"]["properties"]["task_mode"]["enum"], json!(["delegation", "message"]));
    }

    #[test]
    fn dropped_and_unknown_fields_are_refused_with_a_hint_before_transport() {
        for (args, hint) in [
            (json!({"name":"a","groups":["default"]}), "groups was removed"),
            (json!({"name":"a","runtime_backend":"codex"}), "follows from the profile"),
            (json!({"name":"a","permissions":"full"}), "use sandbox"),
            (json!({"name":"a","sandbox_mode":"danger-full-access"}), "use sandbox"),
            (json!({"name":"a","principal":"root"}), "Unknown field `principal`"),
            (json!({"name":"a","pin":"yes"}), "Invalid arguments"),
        ] {
            let mut args = args;
            args["action_id"] = json!("check");
            let result = call(AGENT_CREATE, args.clone(), |_| panic!("invalid input transported"));
            assert_eq!(result["outcome"]["status"], "no_write");
            let detail = result["outcome"]["detail"].as_str().unwrap();
            assert!(detail.contains(hint), "{args}: {detail}");
            assert!(detail.contains("Nothing was sent") && !detail.contains("exact-action retry"), "{detail}");
        }
        let result = call(MEMBER_REPLACE, json!({"action_id":"check","predecessor_cutex_session_id":"cutex.old","policy":"keep_old",
            "successor":{"name":"n","groups":[]}}), |_| panic!("invalid input transported"));
        assert!(result["outcome"]["detail"].as_str().unwrap().contains("groups was removed"), "{result}");
        // Mixing the legacy and sparse shapes is refused too.
        let mixed = call(AGENT_CREATE, json!({"action_id":"check","name":"a","spec":spec(),"start_mode":"bootstrap_only"}), |_| panic!("mixed input transported"));
        assert_eq!(mixed["outcome"]["code"], "invalid_arguments");
    }

    #[test]
    fn legacy_operations_map_to_the_new_tools() {
        for (operation, tool) in [
            ("create", AGENT_CREATE),
            ("close", AGENT_ARCHIVE),
            ("online", MEMBER_RUNTIME),
            ("offline", MEMBER_RUNTIME),
            ("restart", MEMBER_RUNTIME),
            ("replace", MEMBER_REPLACE),
        ] {
            let old = json!({"operation":operation,"action_id":"a","cutex_session_id":"cutex.target"});
            let (mapped, args) = legacy_call(&old).unwrap();
            assert_eq!(mapped, tool);
            assert!(args.get("operation").is_none());
            // Mapping back gives the provider operation the old call named.
            assert_eq!(operation_args(mapped, args)["operation"], operation);
        }
        for old in [
            json!({"operation":"query_managed","action_id":"a"}),
            json!({"operation":"grant_operator","action_id":"a"}),
            json!({"operation":"director_rotate","action_id":"a"}),
            json!({"action_id":"a"}),
            json!("not an object"),
        ] {
            assert!(legacy_call(&old).is_none(), "{old}");
        }
    }

    #[test]
    fn historical_import_or_move_provenance_is_not_current_membership() {
        let mut record = json!({"project_id":null,"created_by_director_session":null,
            "cutex_session_id":"cutex.imported","native_session_id":"native",
            "spec":spec(),"created_at":"2026-09-09T00:00:00Z","retired_at":null});
        for provenance in [Value::Null, json!("historical-other-project")] {
            record["project_id"] = provenance;
            assert!(project_ids_are_consistent(
                &json!({"project_id":"current","agents":[record.clone()]}),
                "current"
            ));
        }
        record["invented_authority"] = json!(true);
        assert!(!project_ids_are_consistent(&record, "current"));
        assert!(!project_ids_are_consistent(
            &json!({"authority":{"project_id":"foreign"}}),
            "current"
        ));
    }
}
