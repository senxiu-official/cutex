use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::sync::OnceLock;

use anyhow::Context;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use serde_json::Value;

use crate::config::atomic::write_private_pretty_json_atomic;
use crate::config::paths::runtime_dir;
use crate::management::v2::native_requests::canonical_request_sha256;

const PENDING_REQUESTS_FILE_NAME: &str = "pending-server-requests.json";
const PENDING_REQUESTS_LOCK_FILE_NAME: &str = "pending-server-requests.lock";

static PENDING_REQUESTS_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct PendingNativeServerRequest {
    pub(crate) cutex_session_id: String,
    pub(crate) runtime_generation: u64,
    pub(crate) native: Value,
    pub(crate) created_at: String,
    #[serde(default)]
    resolution: NativeServerRequestResolution,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_claim: Option<NativeServerResponseClaim>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum NativeServerRequestResolution {
    #[default]
    Pending,
    Resolved,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct NativeServerResponseClaim {
    response_id: String,
    body_sha256: String,
    state: NativeServerResponseClaimState,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum NativeServerResponseClaimState {
    Writing,
    Submitted,
    Indeterminate,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ServerResponseClaimDecision {
    Submit,
    InProgress,
    Deduplicated,
    IdempotencyConflict,
    Resolved,
    NotFound,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingNativeServerRequestStore {
    version: u8,
    #[serde(default)]
    requests: Vec<PendingNativeServerRequest>,
}

pub fn record_request(
    cutex_session_id: &str,
    runtime_generation: u64,
    native: Value,
) -> anyhow::Result<()> {
    validate_native_server_request(&native)?;
    if runtime_generation == 0 {
        anyhow::bail!("native server request runtime generation must be positive");
    }
    let request = PendingNativeServerRequest {
        cutex_session_id: cutex_session_id.to_string(),
        runtime_generation,
        native,
        created_at: Utc::now().to_rfc3339(),
        resolution: NativeServerRequestResolution::Pending,
        response_claim: None,
    };
    mutate_store(|store| {
        let request_id = request_id(&request.native).expect("validated native request id");
        if let Some(existing) = find_request_mut(
            store,
            &request.cutex_session_id,
            request.runtime_generation,
            request_id,
        ) {
            // The app-server sends one request to every subscribed
            // connection under one id: a second cutex process connected to
            // the runtime (near a journal handoff) records the same request.
            if existing.native == request.native {
                return Ok(());
            }
            anyhow::bail!("duplicate native server request id within one runtime generation");
        }
        store.requests.push(request.clone());
        Ok(())
    })
}

pub fn claim_response(
    cutex_session_id: &str,
    runtime_generation: u64,
    response_id: &str,
    canonical_body: &Value,
    native_response: &Value,
) -> anyhow::Result<ServerResponseClaimDecision> {
    if response_id.is_empty() || response_id.chars().count() > 256 {
        anyhow::bail!("responseId must be a non-empty string of at most 256 characters");
    }
    let native_request_id = native_response
        .get("id")
        .filter(|id| !id.is_null())
        .context("native server response omitted id")?;
    validate_request_id(native_request_id)?;
    validate_native_response(native_response)?;
    let body_sha256 = canonical_request_sha256(canonical_body)?;
    mutate_store(|store| {
        claim_response_in_store(
            store,
            cutex_session_id,
            runtime_generation,
            response_id,
            body_sha256,
            native_request_id,
        )
    })
}

fn claim_response_in_store(
    store: &mut PendingNativeServerRequestStore,
    cutex_session_id: &str,
    runtime_generation: u64,
    response_id: &str,
    body_sha256: String,
    native_request_id: &Value,
) -> anyhow::Result<ServerResponseClaimDecision> {
    if let Some(existing) = store.requests.iter().find_map(|request| {
        (request.cutex_session_id == cutex_session_id)
            .then_some(request.response_claim.as_ref())
            .flatten()
            .filter(|claim| claim.response_id == response_id)
    }) {
        if existing.body_sha256 != body_sha256 {
            return Ok(ServerResponseClaimDecision::IdempotencyConflict);
        }
        return Ok(match existing.state {
            NativeServerResponseClaimState::Writing => ServerResponseClaimDecision::InProgress,
            NativeServerResponseClaimState::Submitted => ServerResponseClaimDecision::Deduplicated,
            NativeServerResponseClaimState::Indeterminate => ServerResponseClaimDecision::Resolved,
        });
    }
    let Some(request) = find_request_mut(
        store,
        cutex_session_id,
        runtime_generation,
        native_request_id,
    ) else {
        return Ok(ServerResponseClaimDecision::NotFound);
    };
    if request.resolution == NativeServerRequestResolution::Resolved
        || request.response_claim.is_some()
    {
        return Ok(ServerResponseClaimDecision::Resolved);
    }
    request.response_claim = Some(NativeServerResponseClaim {
        response_id: response_id.to_string(),
        body_sha256,
        state: NativeServerResponseClaimState::Writing,
    });
    Ok(ServerResponseClaimDecision::Submit)
}

pub fn release_definitely_failed_response(
    cutex_session_id: &str,
    runtime_generation: u64,
    response_id: &str,
) -> anyhow::Result<()> {
    mutate_store(|store| {
        if let Some(request) = store.requests.iter_mut().find(|request| {
            request.cutex_session_id == cutex_session_id
                && request.runtime_generation == runtime_generation
                && request
                    .response_claim
                    .as_ref()
                    .is_some_and(|claim| claim.response_id == response_id)
        }) {
            request.response_claim = None;
        }
        Ok(())
    })
}

pub fn mark_response_submitted(
    cutex_session_id: &str,
    runtime_generation: u64,
    response_id: &str,
) -> anyhow::Result<()> {
    update_response_claim(
        cutex_session_id,
        runtime_generation,
        response_id,
        NativeServerResponseClaimState::Submitted,
    )
}

pub fn mark_response_indeterminate(
    cutex_session_id: &str,
    runtime_generation: u64,
    response_id: &str,
) -> anyhow::Result<()> {
    update_response_claim(
        cutex_session_id,
        runtime_generation,
        response_id,
        NativeServerResponseClaimState::Indeterminate,
    )
}

fn update_response_claim(
    cutex_session_id: &str,
    runtime_generation: u64,
    response_id: &str,
    state: NativeServerResponseClaimState,
) -> anyhow::Result<()> {
    mutate_store(|store| {
        let claim = store
            .requests
            .iter_mut()
            .find(|request| {
                request.cutex_session_id == cutex_session_id
                    && request.runtime_generation == runtime_generation
                    && request
                        .response_claim
                        .as_ref()
                        .is_some_and(|claim| claim.response_id == response_id)
            })
            .and_then(|request| request.response_claim.as_mut())
            .context("native server response claim no longer exists")?;
        if claim.state != NativeServerResponseClaimState::Writing {
            anyhow::bail!("native server response claim is no longer writable");
        }
        claim.state = state;
        Ok(())
    })
}

pub fn resolve_request(
    cutex_session_id: &str,
    runtime_generation: u64,
    native_request_id: &Value,
) -> anyhow::Result<bool> {
    validate_request_id(native_request_id)?;
    mutate_store(|store| {
        let Some(request) = store.requests.iter_mut().find(|request| {
            request.cutex_session_id == cutex_session_id
                && request.runtime_generation == runtime_generation
                && request_id(&request.native) == Some(native_request_id)
        }) else {
            return Ok(false);
        };
        request.resolution = NativeServerRequestResolution::Resolved;
        Ok(true)
    })
}

/// A turn ended (`turn/completed`): its still-pending requests can no
/// longer be answered. Marks every pending request of this runtime
/// generation whose `params.turnId` is `turn_id` resolved; returns how many.
pub fn resolve_turn_requests(
    cutex_session_id: &str,
    runtime_generation: u64,
    turn_id: &str,
) -> anyhow::Result<usize> {
    if turn_id.is_empty() {
        return Ok(0);
    }
    let matches = |request: &PendingNativeServerRequest| {
        request.cutex_session_id == cutex_session_id
            && request.runtime_generation == runtime_generation
            && request.resolution == NativeServerRequestResolution::Pending
            && request.native.pointer("/params/turnId").and_then(Value::as_str) == Some(turn_id)
    };
    // Most turns end without a pending request: no store write then.
    if !read_store(|store| Ok(store.requests.iter().any(matches)))? {
        return Ok(0);
    }
    mutate_store(|store| {
        let mut resolved = 0;
        for request in store.requests.iter_mut().filter(|request| matches(request)) {
            request.resolution = NativeServerRequestResolution::Resolved;
            resolved += 1;
        }
        Ok(resolved)
    })
}

/// The recorded native request (claimed or not) with this id, if still kept.
pub fn request_native(
    cutex_session_id: &str,
    runtime_generation: u64,
    native_request_id: &Value,
) -> anyhow::Result<Option<Value>> {
    read_store(|store| {
        Ok(store
            .requests
            .iter()
            .find(|request| {
                request.cutex_session_id == cutex_session_id
                    && request.runtime_generation == runtime_generation
                    && request_id(&request.native) == Some(native_request_id)
            })
            .map(|request| request.native.clone()))
    })
}

pub fn snapshot(cutex_session_id: &str, runtime_generation: u64) -> anyhow::Result<Vec<Value>> {
    read_store(|store| {
        Ok(store
            .requests
            .iter()
            .filter(|request| {
                request.cutex_session_id == cutex_session_id
                    && request.runtime_generation == runtime_generation
                    && request.resolution == NativeServerRequestResolution::Pending
                    && request.response_claim.is_none()
            })
            .map(|request| {
                json!({
                    "runtimeGeneration": request.runtime_generation,
                    "native": request.native,
                })
            })
            .collect())
    })
}

pub fn clear_session(cutex_session_id: &str) -> anyhow::Result<usize> {
    mutate_store(|store| {
        let before = store.requests.len();
        store
            .requests
            .retain(|request| request.cutex_session_id != cutex_session_id);
        Ok(before.saturating_sub(store.requests.len()))
    })
}

fn mutate_store<T>(
    mutate: impl FnOnce(&mut PendingNativeServerRequestStore) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    with_store_lock(|path| {
        let mut store = load_store(path)?;
        let result = mutate(&mut store)?;
        save_store(path, &store)?;
        Ok(result)
    })
}

fn read_store<T>(
    read: impl FnOnce(&PendingNativeServerRequestStore) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    with_store_lock(|path| read(&load_store(path)?))
}

fn with_store_lock<T>(action: impl FnOnce(&Path) -> anyhow::Result<T>) -> anyhow::Result<T> {
    let _guard = PENDING_REQUESTS_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| anyhow::anyhow!("app-server pending-request lock was poisoned"))?;
    let root = runtime_dir()?.join("management-v2");
    fs::create_dir_all(&root)?;
    secure_directory(&root)?;
    let lock_file = open_private_lock(&root.join(PENDING_REQUESTS_LOCK_FILE_NAME))?;
    lock_file.lock()?;
    let result = action(&root.join(PENDING_REQUESTS_FILE_NAME));
    let unlock = lock_file.unlock();
    if result.is_ok() {
        unlock?;
    }
    result
}

fn load_store(path: &Path) -> anyhow::Result<PendingNativeServerRequestStore> {
    match fs::read(path) {
        Ok(data) => {
            let raw: Value = serde_json::from_slice(&data).with_context(|| {
                format!("failed to parse pending server requests {}", path.display())
            })?;
            let contains_legacy_native_response = raw
                .get("requests")
                .and_then(Value::as_array)
                .is_some_and(|requests| {
                    requests.iter().any(|request| {
                        request
                            .get("responseClaim")
                            .is_some_and(|claim| claim.get("nativeResponse").is_some())
                    })
                });
            let store: PendingNativeServerRequestStore =
                serde_json::from_value(raw).with_context(|| {
                    format!("failed to parse pending server requests {}", path.display())
                })?;
            if store.version != 1 {
                anyhow::bail!("unsupported pending server request store version");
            }
            if contains_legacy_native_response {
                save_store(path, &store)?;
            }
            Ok(store)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Ok(PendingNativeServerRequestStore {
                version: 1,
                requests: Vec::new(),
            })
        }
        Err(error) => Err(error)
            .with_context(|| format!("failed to read pending server requests {}", path.display())),
    }
}

fn save_store(path: &Path, store: &PendingNativeServerRequestStore) -> anyhow::Result<()> {
    if store.requests.is_empty() {
        return match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| {
                format!(
                    "failed to remove pending server requests {}",
                    path.display()
                )
            }),
        };
    }
    write_private_pretty_json_atomic(path, store, "pending native server requests")
}

fn find_request_mut<'a>(
    store: &'a mut PendingNativeServerRequestStore,
    cutex_session_id: &str,
    runtime_generation: u64,
    native_request_id: &Value,
) -> Option<&'a mut PendingNativeServerRequest> {
    store.requests.iter_mut().find(|request| {
        request.cutex_session_id == cutex_session_id
            && request.runtime_generation == runtime_generation
            && request_id(&request.native) == Some(native_request_id)
    })
}

fn request_id(native: &Value) -> Option<&Value> {
    native.get("id").filter(|id| !id.is_null())
}

fn validate_native_server_request(native: &Value) -> anyhow::Result<()> {
    let object = native
        .as_object()
        .context("native server request must be an object")?;
    validate_request_id(
        object
            .get("id")
            .filter(|id| !id.is_null())
            .context("native server request omitted id")?,
    )?;
    object
        .get("method")
        .and_then(Value::as_str)
        .filter(|method| !method.is_empty())
        .context("native server request omitted method")?;
    Ok(())
}

fn validate_native_response(native: &Value) -> anyhow::Result<()> {
    let object = native
        .as_object()
        .context("native server response must be an object")?;
    let has_result = object.contains_key("result");
    let has_error = object.contains_key("error");
    if has_result == has_error {
        anyhow::bail!("native server response must contain exactly one of result or error");
    }
    if let Some(error) = object.get("error") {
        let error = error
            .as_object()
            .context("native server response error must be an object")?;
        error
            .get("code")
            .and_then(Value::as_i64)
            .context("native server response error omitted integer code")?;
        error
            .get("message")
            .and_then(Value::as_str)
            .context("native server response error omitted message")?;
    }
    Ok(())
}

fn validate_request_id(request_id: &Value) -> anyhow::Result<()> {
    if request_id.is_string() || request_id.as_i64().is_some() {
        return Ok(());
    }
    anyhow::bail!("native server request id must be a string or signed 64-bit integer")
}

fn open_private_lock(path: &Path) -> anyhow::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    let file = options.open(path)?;
    secure_file(path)?;
    Ok(file)
}

#[cfg(unix)]
fn secure_directory(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn secure_directory(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn secure_file(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn secure_file(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_request_identity_includes_generation_and_preserves_raw_native_message() {
        let native = json!({
            "id": i64::MIN,
            "method": "future/serverRequest",
            "params": { "threadId": "thread-1" },
            "futureField": null
        });
        validate_native_server_request(&native).expect("valid native server request");
        let mut store = PendingNativeServerRequestStore {
            version: 1,
            requests: vec![PendingNativeServerRequest {
                cutex_session_id: "cutex-1".to_string(),
                runtime_generation: 7,
                native: native.clone(),
                created_at: "2026-07-13T00:00:00Z".to_string(),
                resolution: NativeServerRequestResolution::Pending,
                response_claim: None,
            }],
        };
        assert!(find_request_mut(&mut store, "cutex-1", 7, &json!(i64::MIN)).is_some());
        assert!(find_request_mut(&mut store, "cutex-1", 8, &json!(i64::MIN)).is_none());
        assert!(store.requests[0].native.get("futureField").is_some());
    }

    #[test]
    fn turn_end_resolves_its_pending_requests_and_summaries_list_the_rest() {
        // Other tests point HOME (so the store) elsewhere while they run.
        let _home = crate::config::paths::test_home_env_lock();
        let session = format!("cutex.server-request-turn-{}", uuid::Uuid::new_v4());
        let approval = |id: i64, turn: &str| {
            json!({
                "id": id,
                "method": "item/commandExecution/requestApproval",
                "params": { "threadId": "thread-1", "turnId": turn, "itemId": "item" }
            })
        };
        record_request(&session, 2, approval(1, "turn-a")).unwrap();
        record_request(&session, 2, approval(2, "turn-b")).unwrap();
        record_request(&session, 1, approval(3, "turn-a")).unwrap();
        let listed = |generation: u64| {
            pending_summaries().unwrap().remove(&(session.clone(), generation)).unwrap_or_default()
        };
        assert_eq!(
            listed(2),
            vec![
                json!({"id": 1, "method": "item/commandExecution/requestApproval"}),
                json!({"id": 2, "method": "item/commandExecution/requestApproval"}),
            ]
        );
        assert_eq!(resolve_turn_requests(&session, 2, "turn-a").unwrap(), 1);
        assert_eq!(resolve_turn_requests(&session, 2, "turn-a").unwrap(), 0);
        assert_eq!(listed(2), vec![json!({"id": 2, "method": "item/commandExecution/requestApproval"})]);
        // Another runtime generation is untouched.
        assert_eq!(listed(1).len(), 1);
        assert!(resolve_request(&session, 2, &json!(2)).unwrap());
        assert!(listed(2).is_empty());
        clear_session(&session).unwrap();
    }

    #[test]
    fn the_same_request_seen_on_a_second_connection_is_recorded_once() {
        let session = format!("cutex.server-request-twice-{}", uuid::Uuid::new_v4());
        let native = json!({
            "id": 41,
            "method": "item/commandExecution/requestApproval",
            "params": { "threadId": "thread-1" }
        });
        record_request(&session, 3, native.clone()).expect("first connection");
        record_request(&session, 3, native.clone()).expect("second connection: same request");
        let mut different = native;
        different["params"]["threadId"] = json!("thread-2");
        assert!(record_request(&session, 3, different).is_err(), "a different request under one id");
        assert_eq!(clear_session(&session).expect("clear"), 1);
    }

    #[test]
    fn response_claim_is_idempotent_and_first_responder_wins() {
        let native_response = json!({
            "id": "approval-1",
            "result": { "decision": "accept" }
        });
        let body = json!({
            "responseId": "response-1",
            "native": { "message": native_response }
        });
        let body_sha = canonical_request_sha256(&body).expect("hash response body");
        let mut store = PendingNativeServerRequestStore {
            version: 1,
            requests: vec![PendingNativeServerRequest {
                cutex_session_id: "cutex-1".to_string(),
                runtime_generation: 7,
                native: json!({
                    "id": "approval-1",
                    "method": "item/commandExecution/requestApproval",
                    "params": { "threadId": "thread-1" }
                }),
                created_at: "2026-07-13T00:00:00Z".to_string(),
                resolution: NativeServerRequestResolution::Pending,
                response_claim: None,
            }],
        };

        assert_eq!(
            claim_response_in_store(
                &mut store,
                "cutex-1",
                7,
                "response-1",
                body_sha.clone(),
                &json!("approval-1"),
            )
            .expect("first claim"),
            ServerResponseClaimDecision::Submit
        );
        assert_eq!(
            claim_response_in_store(
                &mut store,
                "cutex-1",
                7,
                "response-1",
                body_sha.clone(),
                &json!("approval-1"),
            )
            .expect("in-progress retry"),
            ServerResponseClaimDecision::InProgress
        );
        assert_eq!(
            claim_response_in_store(
                &mut store,
                "cutex-1",
                7,
                "response-1",
                "different".to_string(),
                &json!("approval-1"),
            )
            .expect("idempotency conflict"),
            ServerResponseClaimDecision::IdempotencyConflict
        );
        store.requests[0]
            .response_claim
            .as_mut()
            .expect("response claim")
            .state = NativeServerResponseClaimState::Submitted;
        let persisted = serde_json::to_value(&store).expect("serialize claim store");
        assert!(persisted
            .pointer("/requests/0/responseClaim/bodySha256")
            .is_some());
        assert!(persisted
            .pointer("/requests/0/responseClaim/nativeResponse")
            .is_none());
        assert!(!serde_json::to_string(&persisted)
            .expect("serialize persisted claim")
            .contains("accept"));
        assert!(matches!(
            claim_response_in_store(
                &mut store,
                "cutex-1",
                7,
                "response-1",
                body_sha,
                &json!("approval-1"),
            )
            .expect("deduplicated retry"),
            ServerResponseClaimDecision::Deduplicated
        ));
        assert_eq!(
            claim_response_in_store(
                &mut store,
                "cutex-1",
                7,
                "response-2",
                "new".to_string(),
                &json!("approval-1"),
            )
            .expect("other responder loses"),
            ServerResponseClaimDecision::Resolved
        );
    }

    #[test]
    fn loading_legacy_claim_scrubs_persisted_native_response() {
        let root = std::env::temp_dir().join(format!(
            "cutex-server-request-scrub-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).expect("create scrub directory");
        let path = root.join(PENDING_REQUESTS_FILE_NAME);
        fs::write(
            &path,
            serde_json::to_vec_pretty(&json!({
                "version": 1,
                "requests": [{
                    "cutexSessionId": "cutex-1",
                    "runtimeGeneration": 7,
                    "native": {
                        "id": 1,
                        "method": "item/tool/requestUserInput"
                    },
                    "createdAt": "2026-07-13T00:00:00Z",
                    "resolution": "pending",
                    "responseClaim": {
                        "responseId": "response-secret",
                        "bodySha256": "a".repeat(64),
                        "nativeResponse": {
                            "id": 1,
                            "result": {
                                "answers": {
                                    "secret": {
                                        "answers": ["must-be-scrubbed"]
                                    }
                                }
                            }
                        },
                        "state": "submitted"
                    }
                }]
            }))
            .expect("serialize legacy store"),
        )
        .expect("write legacy store");

        let store = load_store(&path).expect("load and scrub legacy store");
        assert_eq!(store.requests.len(), 1);
        let scrubbed = fs::read_to_string(&path).expect("read scrubbed store");
        assert!(!scrubbed.contains("nativeResponse"));
        assert!(!scrubbed.contains("must-be-scrubbed"));
        assert!(scrubbed.contains("bodySha256"));

        fs::remove_dir_all(root).expect("remove scrub directory");
    }
}

/// `{id, method}` of every pending, unanswered native server request of this
/// host, keyed by `(cutexSessionId, runtimeGeneration)` in arrival order
/// (session resource `runtime.pendingServerRequests`). Same filter as
/// [`snapshot`].
pub fn pending_summaries(
) -> anyhow::Result<std::collections::HashMap<(String, u64), Vec<Value>>> {
    let mut summaries: std::collections::HashMap<(String, u64), Vec<Value>> =
        std::collections::HashMap::new();
    for (session, generation, native, _) in pending_all()? {
        summaries.entry((session, generation)).or_default().push(json!({
            "id": native.get("id").cloned().unwrap_or(Value::Null),
            "method": native.get("method").cloned().unwrap_or(Value::Null),
        }));
    }
    Ok(summaries)
}

/// Every pending native server request of this host as
/// `(cutexSessionId, runtimeGeneration, native message, createdAt)` (host
/// summary). Same filter as [`snapshot`] minus the session.
pub fn pending_all() -> anyhow::Result<Vec<(String, u64, Value, String)>> {
    read_store(|store| {
        Ok(store
            .requests
            .iter()
            .filter(|request| {
                request.resolution == NativeServerRequestResolution::Pending
                    && request.response_claim.is_none()
            })
            .map(|request| {
                (
                    request.cutex_session_id.clone(),
                    request.runtime_generation,
                    request.native.clone(),
                    request.created_at.clone(),
                )
            })
            .collect())
    })
}
