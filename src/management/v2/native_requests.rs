use std::sync::OnceLock;

use serde_json::Map;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;

use super::subagent_lineage::SUBAGENT_READ_METHODS;

pub use super::request_idempotency::request_idempotency_repository;
pub use super::request_idempotency::BeginRequest;
pub use super::request_idempotency::RequestClaim;
pub use super::request_idempotency::RequestIdempotencyRepository;
pub use super::request_idempotency::StoredHttpResponse;

pub const NATIVE_REQUEST_POLICY_VERSION: u8 = 2;

static CLIENT_REQUEST_VALIDATOR: OnceLock<jsonschema::Validator> = OnceLock::new();

const CLIENT_REQUEST_SCHEMA: &str = include_str!("schema/client-request-0.144.1-experimental.json");

/// Digest of the embedded native ClientRequest schema, computed from the bytes
/// actually compiled in (identity for clients; never a hand-maintained pin).
pub fn native_request_policy_sha256() -> String {
    format!("{:x}", Sha256::digest(CLIENT_REQUEST_SCHEMA.as_bytes()))
}

/// Digest of the native method allow rules, computed from the compiled lists.
pub fn native_request_allow_rules_sha256() -> String {
    let rules = serde_json::json!({
        "sessionThread": SESSION_THREAD_METHODS,
        "ownerGlobalRead": OWNER_GLOBAL_READ_METHODS,
        "ownerGlobalFenced": OWNER_GLOBAL_FENCED_METHODS,
    });
    format!("{:x}", Sha256::digest(rules.to_string().as_bytes()))
}

const SESSION_THREAD_METHODS: &[&str] = &[
    "thread/name/set",
    "thread/goal/set",
    "thread/goal/get",
    "thread/goal/clear",
    "thread/metadata/update",
    "thread/settings/update",
    "thread/memoryMode/set",
    "thread/compact/start",
    "thread/backgroundTerminals/list",
    "thread/backgroundTerminals/terminate",
    "thread/read",
    "thread/turns/list",
    "thread/items/list",
    "claude/model/status",
    "turn/start",
    "turn/steer",
    "turn/interrupt",
    "review/start",
];

const OWNER_GLOBAL_READ_METHODS: &[&str] = &[
    "model/list",
    "claude/models/catalog",
    "permissionProfile/list",
    "collaborationMode/list",
    // ChatGPT-account profiles: native rate-limit windows for quota gauges.
    "account/rateLimits/read",
    // Capability `windows-sandbox-readiness`: whether this runtime's native
    // home has a Windows sandbox backend (`ready` | `notConfigured` |
    // `updateRequired`). With none, native asks before every command under
    // a managed filesystem policy (exec_policy: Windows sandbox level
    // Disabled), so the desktop explains frequent approvals with it.
    "windowsSandbox/readiness",
];

/// Owner-only native methods that change the runtime's native home, each
/// behind a Cutex fence ([`windows_sandbox_setup_fence`]). Access stays the
/// owner's (`access.rs` classifies every unlisted native method so).
const OWNER_GLOBAL_FENCED_METHODS: &[&str] = &[
    // Capability `windows-sandbox-readiness`: `{mode: "unelevated"}` only.
    "windowsSandbox/setupStart",
];

/// Cutex fence for native `windowsSandbox/setupStart` (session and host
/// routes). Only the unelevated (restricted-token) mode passes, without
/// `cwd`:
/// - unelevated setup needs no administrator rights: native validates the
///   managed `windows_sandbox_mode` requirement, runs the legacy preflight
///   (capability-SID ACLs on the setup's writable roots; with no `cwd` there
///   are no workspace roots) and persists `[windows] sandbox =
///   "unelevated"` in that native home's `config.toml` (clearing the legacy
///   `features.experimental_windows_sandbox` keys);
/// - elevated setup launches the setup helper through UAC (`runas`) and
///   creates the local `CodexSandboxOffline` / `CodexSandboxOnline` users,
///   a sandbox group and firewall rules: a system change on the host that a
///   remote client cannot see or answer, so it stays a manual step on that
///   host (cute-codex TUI `/setup-default-sandbox`);
/// - `cwd` would grant sandbox write ACLs on an arbitrary directory.
pub(crate) fn windows_sandbox_setup_fence(params: &Value) -> Result<(), String> {
    let object = params
        .as_object()
        .ok_or_else(|| "windowsSandbox/setupStart params must be an object".to_string())?;
    if let Some(key) = object.keys().find(|key| !matches!(key.as_str(), "mode" | "cwd")) {
        return Err(format!("windowsSandbox/setupStart does not accept {key}"));
    }
    if object.get("cwd").is_some_and(|cwd| !cwd.is_null()) {
        return Err(
            "windowsSandbox/setupStart cwd is not allowed: setup grants no workspace ACLs through Cutex"
                .to_string(),
        );
    }
    match object.get("mode").and_then(Value::as_str) {
        Some("unelevated") => Ok(()),
        Some("elevated") => Err(
            "elevated Windows sandbox setup changes the host (UAC, local sandbox users, firewall rules) and is not available through Cutex; run it on that host (cute-codex /setup-default-sandbox)"
                .to_string(),
        ),
        _ => Err("windowsSandbox/setupStart requires mode \"unelevated\"".to_string()),
    }
}

/// Methods added by native r53 (upstream 0.162.1) that the native request
/// gateway deliberately does not forward for any principal: they answer
/// `method_denied` like every unlisted method, and `access.rs` classifies them
/// owner-only. Kept as a reviewed list so a later allowlist edit sees why.
/// - `thread/attachmentOwner/list`: store-wide reverse lookup (no `threadId`)
///   that lists every thread of the native home owning an attachment; not a
///   read of the bound agent.
/// - `thread/prediction/request`: starts model work on a thread (spends the
///   profile's quota); upstream answers "not implemented yet" in 0.162.1.
/// - `account/bedrock/checkGovCloudRequirements`: account / host scope; no
///   cutex profile uses Bedrock.
/// The matching notification `thread/prediction/updated` is passed through
/// like any other bound-thread notification (it carries `threadId`).
pub const NATIVE_R53_UNFORWARDED_METHODS: &[&str] = &[
    "thread/attachmentOwner/list",
    "thread/prediction/request",
    "account/bedrock/checkGovCloudRequirements",
];

#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedNativeRequest {
    pub request_id: String,
    pub native_message: Value,
    pub method: String,
    /// Set when a read-only method (`SUBAGENT_READ_METHODS`) targets a thread
    /// other than the bound one. The caller must verify that it is a
    /// sub-agent (descendant) of the bound thread before forwarding.
    pub descendant_thread_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeRequestValidationError {
    Invalid(String),
    MethodDenied(String),
    MissingThreadId,
    ForeignThread { requested: String, bound: String },
}

pub fn validate_native_request(
    body: &[u8],
    bound_thread_id: &str,
) -> Result<ValidatedNativeRequest, NativeRequestValidationError> {
    let value: Value = serde_json::from_slice(body).map_err(|error| {
        NativeRequestValidationError::Invalid(format!("invalid JSON request body: {error}"))
    })?;
    let object = value.as_object().ok_or_else(|| {
        NativeRequestValidationError::Invalid("request body must be a JSON object".to_string())
    })?;
    require_exact_keys(object, &["requestId", "native"])?;
    let request_id = object
        .get("requestId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.chars().count() <= 256)
        .ok_or_else(|| {
            NativeRequestValidationError::Invalid(
                "requestId must be a non-empty string of at most 256 characters".to_string(),
            )
        })?
        .to_string();
    let native = object
        .get("native")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            NativeRequestValidationError::Invalid("native must be an object".to_string())
        })?;
    require_exact_keys(native, &["message"])?;
    let native_message = native.get("message").cloned().ok_or_else(|| {
        NativeRequestValidationError::Invalid("native.message is required".to_string())
    })?;
    let message = native_message.as_object().ok_or_else(|| {
        NativeRequestValidationError::Invalid("native.message must be an object".to_string())
    })?;
    let id = message
        .get("id")
        .filter(|id| !id.is_null())
        .ok_or_else(|| {
            NativeRequestValidationError::Invalid("native.message.id is required".to_string())
        })?;
    if !id.is_string() && id.as_i64().is_none() {
        return Err(NativeRequestValidationError::Invalid(
            "native.message.id must be a string or signed 64-bit integer".to_string(),
        ));
    }
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .filter(|method| !method.is_empty() && !method.starts_with("cutex/"))
        .ok_or_else(|| {
            NativeRequestValidationError::Invalid(
                "native.message.method must be a non-empty native method".to_string(),
            )
        })?
        .to_string();

    let mut descendant_thread_id = None;
    if SESSION_THREAD_METHODS.contains(&method.as_str()) {
        let thread_id = message
            .get("params")
            .and_then(Value::as_object)
            .and_then(|params| params.get("threadId"))
            .and_then(Value::as_str)
            .filter(|thread_id| !thread_id.is_empty())
            .ok_or(NativeRequestValidationError::MissingThreadId)?;
        if thread_id != bound_thread_id {
            // Read-only methods may target a sub-agent thread of the bound
            // thread; the caller verifies that lineage before forwarding.
            // Every other session method stays pinned to the bound thread.
            if !SUBAGENT_READ_METHODS.contains(&method.as_str()) {
                return Err(NativeRequestValidationError::ForeignThread {
                    requested: thread_id.to_string(),
                    bound: bound_thread_id.to_string(),
                });
            }
            descendant_thread_id = Some(thread_id.to_string());
        }
    } else if OWNER_GLOBAL_FENCED_METHODS.contains(&method.as_str()) {
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        windows_sandbox_setup_fence(&params).map_err(NativeRequestValidationError::Invalid)?;
    } else if !OWNER_GLOBAL_READ_METHODS.contains(&method.as_str()) {
        return Err(NativeRequestValidationError::MethodDenied(method));
    }
    // The catalog may include a selected model from this bound thread, but
    // may never inspect another session's account/model selection.
    if method == "claude/models/catalog" {
        if let Some(thread_id) = message.get("params").and_then(|params| params.get("threadId")).and_then(Value::as_str) {
            if thread_id != bound_thread_id {
                return Err(NativeRequestValidationError::ForeignThread {
                    requested: thread_id.to_string(), bound: bound_thread_id.to_string(),
                });
            }
        }
    }
    validate_pinned_client_request_schema(&native_message)?;

    Ok(ValidatedNativeRequest {
        request_id,
        native_message,
        method,
        descendant_thread_id,
    })
}

pub(super) fn validate_pinned_client_request_schema(
    native_message: &Value,
) -> Result<(), NativeRequestValidationError> {
    if matches!(native_message["method"].as_str(), Some("claude/models/catalog" | "claude/model/status")) {
        return validate_claude_model_read(native_message);
    }
    let validator = CLIENT_REQUEST_VALIDATOR.get_or_init(|| {
        let schema: Value = serde_json::from_str(CLIENT_REQUEST_SCHEMA)
            .expect("embedded app-server 0.144.1 ClientRequest schema must be valid JSON");
        jsonschema::options()
            .with_draft(jsonschema::Draft::Draft7)
            .build(&schema)
            .expect("embedded app-server 0.144.1 ClientRequest schema must compile")
    });
    if let Err(error) = validator.validate(native_message) {
        return Err(NativeRequestValidationError::Invalid(format!(
            "native.message does not match app-server 0.144.1 experimental ClientRequest: {error}"
        )));
    }
    Ok(())
}

/// Claude extensions have their own deliberately narrow read-only shape;
/// they are not invented entries in the pinned Codex ClientRequest schema.
fn validate_claude_model_read(message: &Value) -> Result<(), NativeRequestValidationError> {
    let invalid = || NativeRequestValidationError::Invalid("Claude model reads accept only an optional catalog threadId, or a required status threadId".into());
    let object = message.as_object().ok_or_else(invalid)?;
    if object.keys().any(|key| !matches!(key.as_str(), "id" | "method" | "params" | "jsonrpc"))
        || object.get("jsonrpc").is_some_and(|value| value != "2.0") {
        return Err(invalid());
    }
    let status = message["method"] == "claude/model/status";
    let Some(params) = object.get("params") else { return if status { Err(invalid()) } else { Ok(()) }; };
    let params = params.as_object().ok_or_else(invalid)?;
    if params.keys().any(|key| key != "threadId")
        || params.get("threadId").is_some_and(|id| id.as_str().is_none_or(str::is_empty))
        || (status && !params.contains_key("threadId")) {
        return Err(invalid());
    }
    Ok(())
}

fn require_exact_keys(
    object: &Map<String, Value>,
    expected: &[&str],
) -> Result<(), NativeRequestValidationError> {
    if object.len() != expected.len() || expected.iter().any(|key| !object.contains_key(*key)) {
        return Err(NativeRequestValidationError::Invalid(format!(
            "object must contain exactly: {}",
            expected.join(", ")
        )));
    }
    Ok(())
}

pub fn canonical_request_sha256(value: &Value) -> anyhow::Result<String> {
    let canonical = canonicalize(value);
    let bytes = serde_json::to_vec(&canonical)?;
    let digest = Sha256::digest(bytes);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| (key.clone(), canonicalize(value)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(canonicalize).collect()),
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use serde_json::json;
    use uuid::Uuid;

    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("cutex-v2-{label}-{}", Uuid::new_v4()))
    }

    #[test]
    fn claude_model_reads_are_narrow_and_bound_to_the_current_session() {
        let validate = |method: &str, params: Value| {
            let body = serde_json::to_vec(&serde_json::json!({"requestId": "read", "native": {
                "message": {"id": 1, "method": method, "params": params}
            }})).unwrap();
            validate_native_request(&body, "bound")
        };
        assert!(validate("claude/models/catalog", json!({})).is_ok());
        assert!(validate("claude/models/catalog", json!({"threadId": "bound"})).is_ok());
        assert!(validate("claude/model/status", json!({"threadId": "bound"})).is_ok());
        for method in ["claude/models/catalog", "claude/model/status"] {
            assert!(matches!(validate(method, json!({"threadId": "other"})), Err(NativeRequestValidationError::ForeignThread { .. })));
            assert!(validate(method, json!({"threadId": "bound", "model": "mutation"})).is_err());
            assert!(validate(method, Value::Null).is_err());
        }
        assert!(matches!(validate("claude/model/status", json!({})), Err(NativeRequestValidationError::MissingThreadId)));
        assert!(matches!(validate("claude/model/set", json!({"threadId": "bound"})), Err(NativeRequestValidationError::MethodDenied(_))));
    }

    #[test]
    fn native_policy_preserves_message_and_enforces_thread_scope() {
        let body = serde_json::to_vec(&json!({
            "requestId": "request-1",
            "native": {
                "message": {
                    "id": i64::MIN,
                    "method": "thread/read",
                    "params": { "threadId": "thread-1", "includeTurns": true },
                    "futureField": null
                }
            }
        }))
        .expect("serialize request");
        let request = validate_native_request(&body, "thread-1").expect("valid request");
        assert_eq!(request.native_message["id"], i64::MIN);
        assert!(request.native_message.get("futureField").is_some());
        assert_eq!(request.descendant_thread_id, None);
        // Another thread is only a lineage candidate, never trusted outright.
        assert_eq!(
            validate_native_request(&body, "thread-2")
                .expect("candidate")
                .descendant_thread_id
                .as_deref(),
            Some("thread-1")
        );
    }

    fn native_request(method: &str, params: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "requestId": "request-1",
            "native": { "message": { "id": "native-1", "method": method, "params": params } }
        }))
        .expect("serialize request")
    }

    #[test]
    fn named_model_configuration_switch_is_forwarded_without_rewriting() {
        for model in ["Astra [400K]", "Astra [285K]"] {
            let request = validate_native_request(&native_request("thread/settings/update",
                json!({"threadId": "root", "model": model})), "root").unwrap();
            assert_eq!(request.native_message["params"]["model"], model);
        }
        assert!(validate_native_request(&native_request("model/list", json!({})), "root").is_ok());
    }

    #[test]
    fn native_policy_marks_other_thread_reads_for_lineage_verification() {
        let read = validate_native_request(
            &native_request("thread/read", json!({ "threadId": "child", "includeTurns": false })),
            "root",
        )
        .expect("read-only method may target a candidate sub-agent thread");
        assert_eq!(read.descendant_thread_id.as_deref(), Some("child"));
        for (method, params) in [
            ("thread/turns/list", json!({ "threadId": "child" })),
            ("thread/items/list", json!({ "threadId": "child" })),
            ("thread/backgroundTerminals/list", json!({ "threadId": "child" })),
        ] {
            let request = validate_native_request(&native_request(method, params), "root")
                .unwrap_or_else(|error| panic!("{method}: {error:?}"));
            assert_eq!(request.descendant_thread_id.as_deref(), Some("child"));
        }
        let bound = validate_native_request(
            &native_request("thread/read", json!({ "threadId": "root" })),
            "root",
        )
        .expect("bound thread read");
        assert_eq!(bound.descendant_thread_id, None);
    }

    #[test]
    fn native_policy_rejects_write_methods_on_any_other_thread() {
        for (method, params) in [
            ("turn/start", json!({ "threadId": "child", "input": [] })),
            ("turn/steer", json!({ "threadId": "child", "input": [], "expectedTurnId": "t" })),
            ("turn/interrupt", json!({ "threadId": "child", "turnId": "t" })),
            ("thread/name/set", json!({ "threadId": "child", "name": "x" })),
            ("thread/compact/start", json!({ "threadId": "child" })),
            ("thread/backgroundTerminals/terminate", json!({ "threadId": "child", "processId": "p" })),
            ("review/start", json!({ "threadId": "child", "target": { "type": "uncommittedChanges" } })),
        ] {
            assert!(
                matches!(
                    validate_native_request(&native_request(method, params), "root"),
                    Err(NativeRequestValidationError::ForeignThread { .. })
                ),
                "{method} must stay pinned to the bound thread"
            );
        }
    }

    #[test]
    fn windows_sandbox_readiness_and_unelevated_setup_are_forwarded() {
        let body = |method: &str, params: Option<Value>| {
            let mut message = json!({ "id": 7, "method": method });
            if let Some(params) = params {
                message["params"] = params;
            }
            serde_json::to_vec(&json!({ "requestId": "r", "native": { "message": message } })).unwrap()
        };
        let readiness = validate_native_request(&body("windowsSandbox/readiness", None), "root")
            .expect("readiness takes no params and no threadId");
        assert_eq!(readiness.method, "windowsSandbox/readiness");
        assert_eq!(readiness.descendant_thread_id, None);
        // The pinned schema: readiness params must be null / absent.
        assert!(matches!(
            validate_native_request(&body("windowsSandbox/readiness", Some(json!({ "x": 1 }))), "root"),
            Err(NativeRequestValidationError::Invalid(_))
        ));
        assert!(validate_native_request(
            &body("windowsSandbox/setupStart", Some(json!({ "mode": "unelevated" }))),
            "root"
        )
        .is_ok());
        assert!(validate_native_request(
            &body("windowsSandbox/setupStart", Some(json!({ "mode": "unelevated", "cwd": null }))),
            "root"
        )
        .is_ok());
        for params in [
            json!({ "mode": "elevated" }),
            json!({ "mode": "unelevated", "cwd": "C:\\work" }),
            json!({ "mode": "unelevated", "threadId": "root" }),
            json!({}),
            json!("unelevated"),
        ] {
            assert!(
                matches!(
                    validate_native_request(&body("windowsSandbox/setupStart", Some(params.clone())), "root"),
                    Err(NativeRequestValidationError::Invalid(_))
                ),
                "{params}"
            );
        }
        assert!(matches!(
            validate_native_request(&body("windowsSandbox/setupStart", None), "root"),
            Err(NativeRequestValidationError::Invalid(_))
        ));
        let elevated = windows_sandbox_setup_fence(&json!({ "mode": "elevated" })).unwrap_err();
        assert!(elevated.contains("/setup-default-sandbox"), "{elevated}");
    }

    #[test]
    fn native_r53_additions_are_not_forwarded() {
        for method in NATIVE_R53_UNFORWARDED_METHODS {
            assert!(!SESSION_THREAD_METHODS.contains(method), "{method}");
            assert!(!OWNER_GLOBAL_READ_METHODS.contains(method), "{method}");
            assert!(!OWNER_GLOBAL_FENCED_METHODS.contains(method), "{method}");
            assert!(!SUBAGENT_READ_METHODS.contains(method), "{method}");
            let body = serde_json::to_vec(&json!({
                "requestId": "request-1",
                "native": { "message": { "id": 1, "method": method,
                    "params": { "threadId": "thread-1", "sourceTurnId": "turn-1",
                                "attachmentType": "file", "identityKey": "k" } } }
            }))
            .expect("serialize request");
            assert!(matches!(
                validate_native_request(&body, "thread-1"),
                Err(NativeRequestValidationError::MethodDenied(denied)) if denied == *method
            ));
        }
    }

    #[test]
    fn native_policy_denies_unknown_and_unsigned_overflow_ids() {
        let unknown = serde_json::to_vec(&json!({
            "requestId": "request-1",
            "native": { "message": { "id": "native-1", "method": "future/write" } }
        }))
        .expect("serialize request");
        assert!(matches!(
            validate_native_request(&unknown, "thread-1"),
            Err(NativeRequestValidationError::MethodDenied(method)) if method == "future/write"
        ));

        let overflow = format!(
            r#"{{"requestId":"request-1","native":{{"message":{{"id":{},"method":"model/list"}}}}}}"#,
            u64::MAX
        );
        assert!(matches!(
            validate_native_request(overflow.as_bytes(), "thread-1"),
            Err(NativeRequestValidationError::Invalid(_))
        ));
    }

    #[test]
    fn native_policy_uses_exact_pinned_method_parameter_schema() {
        let malformed = serde_json::to_vec(&json!({
            "requestId": "request-1",
            "native": {
                "message": {
                    "id": "native-1",
                    "method": "thread/read",
                    "params": { "threadId": "thread-1", "includeTurns": "yes" }
                }
            }
        }))
        .expect("serialize malformed request");
        assert!(matches!(
            validate_native_request(&malformed, "thread-1"),
            Err(NativeRequestValidationError::Invalid(message))
                if message.contains("ClientRequest")
        ));

    }

    #[test]
    fn idempotency_is_durable_and_restart_uncertainty_is_not_replayed() {
        let root = temp_root("idempotency");
        let request = json!({ "requestId": "request-1", "native": { "message": {} } });
        let first = RequestIdempotencyRepository::open(&root, "process-1".to_string())
            .expect("open first repository");
        let BeginRequest::Forward(claim) = first
            .begin("session-1", "request-1", &request)
            .expect("begin request")
        else {
            panic!("expected forwarding claim");
        };
        assert_eq!(
            first
                .begin("session-1", "request-1", &request)
                .expect("repeat in process"),
            BeginRequest::InProgress
        );
        first
            .complete(
                &claim,
                200,
                "OK",
                json!({ "contractVersion": 2, "ok": true }),
            )
            .expect("complete request");
        assert!(matches!(
            first.begin("session-1", "request-1", &request).expect("replay"),
            BeginRequest::Completed(response) if response.body["ok"] == true
        ));

        let BeginRequest::Forward(_uncertain_claim) = first
            .begin("session-1", "request-2", &json!({ "value": 1 }))
            .expect("begin uncertain request")
        else {
            panic!("expected second forwarding claim");
        };
        drop(first);
        let restarted = RequestIdempotencyRepository::open(&root, "process-2".to_string())
            .expect("open restarted repository");
        assert_eq!(
            restarted
                .begin("session-1", "request-2", &json!({ "value": 1 }))
                .expect("detect uncertainty"),
            BeginRequest::OutcomeUnknown
        );
        assert_eq!(
            restarted
                .begin("session-1", "request-1", &json!({ "different": true }))
                .expect("detect conflict"),
            BeginRequest::Conflict
        );
        drop(restarted);
        fs::remove_dir_all(root).expect("remove temp root");
    }
}
