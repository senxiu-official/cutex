//! Test hook (054 S14–S18; debug builds only, never compiled into release):
//! delegations to and from a seeded, unreviewed app-server runtime (the
//! input-ledger E2E fake Codex) over its `thread/externalInput/{submit,status}`
//! surface, without the reviewed stock bundle `ExternalInputClient` requires.
//!
//! Gated like C3's bus hook (`bus_bridge_external::test_fake_external_input`):
//! `CUTEX_TEST_FAKE_EXTERNAL_INPUT=1` and a private test home
//! (`CUTEX_TEST_PRIVATE_HOME` == `HOME`, with its `.cutex-test-private-home`
//! marker), and only for a plain Codex app-server session (no Claude / DSH
//! session, no explicit stock launch: those keep their reviewed paths). Each
//! entry point answers `None` when the hook does not apply, so the caller
//! continues on its normal path.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::Value;

use crate::app_server::client::{AppServerClient, AppServerClientError, AppServerClientOptions};
use crate::app_server::external_input::{
    DeliveryState, HoldReason, ProcessingState, Response, EXCLUSIVE_TURN_ADMISSION,
};
use crate::asks::scheduler::{NativeMessageStatus, SubmitOutcome, Submission};
use crate::launch::stock::ExternalInputBinding;

/// The env switch plus the private test home marker.
pub fn enabled() -> bool {
    if std::env::var("CUTEX_TEST_FAKE_EXTERNAL_INPUT").as_deref() != Ok("1") {
        return false;
    }
    let (Ok(private), Ok(home)) = (std::env::var("CUTEX_TEST_PRIVATE_HOME"), std::env::var("HOME")) else {
        return false;
    };
    private == home && std::path::Path::new(&private).join(".cutex-test-private-home").is_file()
}

struct Connection {
    generation: u64,
    binding: ExternalInputBinding,
    client: AppServerClient,
}

fn connections() -> &'static Mutex<HashMap<String, Connection>> {
    static CONNECTIONS: OnceLock<Mutex<HashMap<String, Connection>>> = OnceLock::new();
    CONNECTIONS.get_or_init(Default::default)
}

/// Run `use_connection` on the session's fake connection (opened on first
/// use, per runtime generation; dropped after any error). `None`: the hook
/// does not apply to this session.
fn with_connection<T>(
    session: &str,
    generation: u64,
    use_connection: impl FnOnce(&Connection) -> anyhow::Result<T>,
) -> Option<anyhow::Result<T>> {
    if !enabled() {
        return None;
    }
    let record = crate::session::store::with_cutex_session_store(|store| store.sessions.get(session).cloned())
        .ok()
        .flatten()?;
    if record.claude_session.is_some() || record.dsh_session.is_some() || record.explicit_launch.is_some() {
        return None;
    }
    let runtime = record.app_server_runtime.as_ref()?;
    let thread_id = record.codex_session_id.clone()?;
    let mut connections = connections().lock().ok()?;
    if connections.get(session).is_some_and(|c| c.generation != generation) {
        connections.remove(session);
    }
    if !connections.contains_key(session) {
        let opened = (|| -> anyhow::Result<Connection> {
            anyhow::ensure!(record.runtime_generation == generation, "runtime_generation_changed");
            let endpoint = crate::app_server::runtime::endpoint_from_runtime_binding(runtime)?;
            let client = AppServerClient::connect_rpc_only(AppServerClientOptions::new(endpoint))?;
            anyhow::ensure!(
                client.initialize_response().get("externalInputVersion").and_then(Value::as_u64) == Some(1),
                "test fake: no externalInput surface"
            );
            let binding = ExternalInputBinding {
                version: 1,
                owner_id: session.to_string(),
                thread_id,
                runtime_generation: generation,
                canonical_byte_limit: Default::default(),
            };
            Ok(Connection { generation, binding, client })
        })();
        match opened {
            Ok(connection) => {
                connections.insert(session.to_string(), connection);
            }
            Err(error) => return Some(Err(error)),
        }
    }
    let result = use_connection(connections.get(session)?);
    if result.is_err() {
        connections.remove(session);
    }
    Some(result)
}

fn capability(connection: &Connection, key: &str) -> Option<u64> {
    connection.client.initialize_response().get(key).and_then(Value::as_u64)
}

fn takes_soon(connection: &Connection) -> bool {
    connection
        .client
        .initialize_response()
        .get("externalInputDeliveries")
        .and_then(Value::as_array)
        .is_some_and(|modes| modes.iter().any(|m| m.as_str() == Some("soon")))
}

fn busy_reason(error: &AppServerClientError) -> Option<String> {
    let AppServerClientError::Rpc(rpc) = error else { return None };
    let data = rpc.data.as_ref()?;
    (data.get("externalInputError").and_then(Value::as_str) == Some("busy"))
        .then(|| data.get("reason").and_then(Value::as_str).unwrap_or("active_turn").to_string())
}

/// `TargetStatus.exclusive_admission` (N1): the fake reports
/// `externalInputControlVersion >= 1`.
pub fn supports_exclusive_turn(session: &str, generation: u64) -> Option<bool> {
    with_connection(session, generation, |c| Ok(capability(c, "externalInputControlVersion").is_some_and(|v| v >= 1)))
        .map(|result| result.unwrap_or(false))
}

/// `AskRuntime::takes_next_step`: the fake lists `soon`.
pub fn supports_soon(session: &str, generation: u64) -> Option<bool> {
    with_connection(session, generation, |c| Ok(takes_soon(c))).map(|result| result.unwrap_or(false))
}

/// `AskRuntime::submit_exclusive` (N1 exclusive admission).
pub fn submit_exclusive(session: &str, generation: u64, submission: &Submission) -> Option<SubmitOutcome> {
    let result = with_connection(session, generation, |c| {
        let envelope = submission.envelope(&c.binding, false)?;
        let mut params = serde_json::to_value(&envelope)?;
        params["admission"] = Value::from(EXCLUSIVE_TURN_ADMISSION);
        let mut raw = match c.client.handle().request("thread/externalInput/submit", params) {
            Ok(raw) => raw,
            Err(error) => {
                return Ok(match busy_reason(&error) {
                    Some(reason) => SubmitOutcome::Busy { reason },
                    None => SubmitOutcome::Uncertain(error.to_string()),
                })
            }
        };
        let turn_id = raw
            .as_object_mut()
            .and_then(|object| object.remove("turnId"))
            .and_then(|value| value.as_str().map(str::to_string));
        let response: Response = serde_json::from_value(raw)?;
        response.validate(&c.binding, &[envelope.key()])?;
        let status = response.statuses.into_iter().next();
        let turn_id = turn_id.or_else(|| status.and_then(|s| s.receipt.map(|r| r.turn_id)));
        Ok(match turn_id {
            Some(turn_id) => SubmitOutcome::Started { turn_id },
            None => SubmitOutcome::Uncertain("accepted without receipt; reconciling".into()),
        })
    })?;
    Some(result.unwrap_or_else(|error| SubmitOutcome::Unsupported(format!("runtime unavailable: {error:#}"))))
}

/// `AskRuntime::steer_note` (a `soon` result for a busy requester).
pub fn steer_note(session: &str, generation: u64, submission: &Submission) -> Option<SubmitOutcome> {
    let result = with_connection(session, generation, |c| {
        anyhow::ensure!(takes_soon(c), "native ingress does not support soon");
        let mut envelope = submission.envelope(&c.binding, false)?;
        envelope.message.delivery = crate::app_server::external_input::Delivery::Soon;
        envelope.semantic_sha256 = envelope.digest();
        let raw = c.client.handle().request("thread/externalInput/submit", serde_json::to_value(&envelope)?)?;
        let response: Response = serde_json::from_value(raw)?;
        response.validate(&c.binding, &[envelope.key()])?;
        Ok(SubmitOutcome::Started { turn_id: String::new() })
    })?;
    Some(result.unwrap_or_else(|error| SubmitOutcome::Unsupported(format!("runtime unavailable: {error:#}"))))
}

/// `AskRuntime::message_status`.
pub fn message_status(session: &str, generation: u64, submission: &Submission) -> Option<NativeMessageStatus> {
    let result = with_connection(session, generation, |c| {
        let key = submission.envelope(&c.binding, false)?.key();
        let mut params = serde_json::json!({"version": 1, "ownerId": c.binding.owner_id,
            "threadId": c.binding.thread_id, "runtimeGeneration": c.binding.runtime_generation});
        params["messages"] = serde_json::to_value([&key])?;
        let response: Response = serde_json::from_value(c.client.handle().request("thread/externalInput/status", params)?)?;
        response.validate(&c.binding, &[key])?;
        response.statuses.into_iter().next().ok_or_else(|| anyhow::anyhow!("empty status"))
    })?;
    let status = match result {
        Ok(status) => status,
        Err(error) => return Some(NativeMessageStatus::Unavailable(format!("{error:#}"))),
    };
    let turn_id = status.receipt.as_ref().map(|receipt| receipt.turn_id.clone());
    if status.processing.state == ProcessingState::Held {
        let reason = status
            .processing
            .reason
            .as_ref()
            .and_then(|reason: &HoldReason| serde_json::to_value(reason).ok())
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_default();
        return Some(NativeMessageStatus::Held { reason, turn_id });
    }
    Some(match (status.delivery_state, turn_id) {
        (_, Some(turn_id)) => NativeMessageStatus::Persisted { turn_id },
        (DeliveryState::Unknown, None) => NativeMessageStatus::Absent,
        (DeliveryState::Pending, None) => NativeMessageStatus::Pending,
        (other, None) => NativeMessageStatus::Unavailable(format!("{other:?}")),
    })
}
