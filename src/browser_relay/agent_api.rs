//! cxa1-only tool transport. SQLite owns replay admission; this process owns
//! live deadlines/results. No caller can mint a device, lease, or new identity.
use super::{
    auth,
    commands::{Admission, AgentCommand, Authority, Plan, Receipt, Status, COMMAND_TIMEOUT},
    model::*,
    state::{self, Relay},
};
use crate::{
    http::server::SimpleHttpRequest,
    management::{
        server::ManagementRequestContext,
        v2::request_idempotency::{self, BeginRequest, RequestClaim, RequestIdempotencyRepository},
    },
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    net::TcpStream,
    sync::Arc,
    time::{Duration, Instant},
};

pub struct Reply {
    pub status: u16,
    pub value: Arc<Value>,
    pub delivered: Option<String>,
    pub retained: Option<Arc<super::retained::Body>>,
}
impl Reply {
    fn success(value: Value) -> Self {
        Self {
            status: 200,
            value: Arc::new(value),
            delivered: None,
            retained: None,
        }
    }
    fn error(error: RelayError, approval: Option<&str>) -> Self {
        let mut value = error.value();
        if let Some(id) = approval {
            value["error"]["details"]["approvalId"] = json!(id);
        }
        Self {
            status: error.status,
            value: Arc::new(value),
            delivered: None,
            retained: None,
        }
    }
    fn capacity(full: request_idempotency::CapacityExceeded) -> Self {
        let mut value = RelayError::new("idempotency_capacity_exceeded").value();
        value["error"]["details"] = full.details();
        Self { status: 503, value: Arc::new(value), delivered: None, retained: None }
    }
}
fn repository() -> Result<&'static RequestIdempotencyRepository> {
    request_idempotency::request_idempotency_repository()
        .map_err(|_| RelayError::new("browser_unavailable"))
}
fn prefix(agent: &auth::AgentContext) -> String {
    format!(
        "browser:{:x}:",
        Sha256::digest(format!("{}\0{}", agent.credential_identity, agent.session).as_bytes())
    )
}
fn key(scope: &str, id: &str) -> String {
    format!("{scope}\0{id}")
}
fn scope(agent: &auth::AgentContext, device: &str) -> String {
    format!("{}session:{:x}:{device}", prefix(agent), Sha256::digest(agent.session.as_bytes()))
}
fn canonical(input: &AgentCommand) -> Value {
    json!({"tool":input.tool,"arguments":input.arguments})
}
fn persisted_reply(metadata: &Value) -> Reply {
    let code = metadata
        .pointer("/error/code")
        .and_then(Value::as_str)
        .map(RelayError::provider)
        .unwrap_or(RelayError::new("request_result_not_retained"));
    Reply::error(code, metadata["approvalId"].as_str())
}
fn complete(
    repo: &RequestIdempotencyRepository,
    claim: &RequestClaim,
    status: Status,
    code: Option<RelayError>,
    approval: Option<&str>,
) -> Result<()> {
    let mut metadata = json!({"status":status});
    if let Some(code) = code {
        metadata["error"] = json!({"code":code.code});
    }
    if let Some(id) = approval {
        metadata["approvalId"] = json!(id);
    }
    repo.complete(
        claim,
        if code.is_some() { 400 } else { 200 },
        "Browser relay result",
        metadata,
    )
    .map_err(|_| RelayError::new("browser_unavailable"))
}
fn immediate_error(
    repo: &RequestIdempotencyRepository,
    claim: &RequestClaim,
    error: RelayError,
    approval: Option<&str>,
) -> Result<Reply> {
    // Only before queue admission (or a direct operation's settlement failure,
    // whose once-only claim already preserves its unresolved tombstone).
    repo.release_browser_refusal(claim)
        .map_err(|_| RelayError::new("browser_unavailable"))?;
    Ok(Reply::error(error, approval))
}
fn deterministic_refusal(receipt: &Receipt) -> bool {
    match receipt.status {
        Status::Failed | Status::Cancelled if !receipt.dispatched => true,
        // Executor tab.act maps any post-input error to outcome_unknown;
        // these Failed codes therefore prove no page input. Navigation/open
        // have different side effects: never apply this list to those kinds.
        Status::Failed => receipt.code.is_some_and(|e| {
            e.code == "unsupported_operation"
                || receipt.operation_kind == "tab.act" && matches!(e.code,
                    "permission_denied" | "target_hidden" | "owner_paused" | "invalid_request")
                // Provenance registration fails before guest navigation.
                || receipt.operation_kind == "tabs.open" && e.code == "browser_identity_unavailable"
        }),
        _ => false,
    }
}
fn settle_receipt(repo: &RequestIdempotencyRepository, receipt: &Receipt) -> Result<()> {
    let Some(claim) = &receipt.claim else { return Ok(()); };
    if deterministic_refusal(receipt) {
        repo.release_browser_refusal(claim)
            .map_err(|_| RelayError::new("browser_unavailable"))
    } else {
        complete(repo, claim, receipt.status, receipt.code, None)
    }
}
fn settle_admission(repo: &RequestIdempotencyRepository, claim: &RequestClaim, mut receipt: Receipt) -> Result<()> {
    // A replay can re-admit after a refusal row was removed while the in-memory
    // ledger still retains the old terminal command. Settle this admission's
    // reservation too; the ledger must neither redispatch nor leak a new row.
    receipt.claim = Some(claim.clone());
    settle_receipt(repo, &receipt)
}
fn choose_device(relay: &Arc<Relay>) -> Result<(String, String, String)> {
    let state = relay
        .state
        .lock()
        .map_err(|_| RelayError::new("browser_unavailable"))?;
    let device = state::preferred_device(&state).ok_or(RelayError::new("browser_unavailable"))?;
    let identity = device.identity.clone();
    let selected = (
        device.registration.device_id.clone(),
        device.registration.host_id.clone(),
        device.generation.clone(),
    );
    drop(state);
    identity.check()?;
    Ok(selected)
}
pub fn submit(
    relay: &Arc<Relay>,
    request: &SimpleHttpRequest,
    context: ManagementRequestContext,
    stream: &TcpStream,
) -> Result<Reply> {
    let started = Instant::now();
    let agent = auth::agent(request, context, false)?;
    super::http::sweep_authenticated(relay);
    if !crate::agent_bus::tool_policy::browser_tools_enabled(&agent.session) {
        return Err(RelayError::new("browser_disabled"));
    }
    let input: AgentCommand = parse(&request.body)?;
    input.validate()?;
    let canonical = canonical(&input);
    let digest = crate::management::v2::native_requests::canonical_request_sha256(&canonical)
        .map_err(|_| RelayError::new("invalid_request"))?;
    let repo = repository()?;
    let prefix = prefix(&agent);
    let old = repo
        .browser_lookup(&prefix, &input.request_id)
        .map_err(|_| RelayError::new("browser_unavailable"))?;
    if old.len() > 1 {
        return Err(RelayError::new("request_conflict"));
    }
    if let Some(old) = old.first() {
        if old.body_sha256 != digest {
            return Err(RelayError::new("request_conflict"));
        }
        let key = key(&old.scope, &input.request_id);
        if relay.receipt(&key).is_ok() {
            return wait(relay, repo, &key, stream, false);
        }
        return Ok(if let Some(metadata) = &old.metadata {
            persisted_reply(metadata)
        } else {
            Reply::error(RelayError::new("outcome_unknown"), None)
        });
    }
    if !identifier(&agent.turn) {
        return Err(RelayError::new("permission_denied"));
    }
    let plan = if matches!(
        input.tool.as_str(),
        "browser_list" | "browser_release" | "browser_claim"
    ) {
        None
    } else {
        Some(relay.plan(&agent, &input)?)
    };
    let selected = if input.tool == "browser_claim" {
        Some(choose_device(relay)?)
    } else {
        None
    };
    let device = if input.tool == "browser_list" {
        "none".into()
    } else if input.tool == "browser_release" {
        let state = relay
            .state
            .lock()
            .map_err(|_| RelayError::new("browser_unavailable"))?;
        state
            .leases
            .values()
            .find(|l| {
                l.view.cutex_session_id == agent.session
                    && l.view.target.guest_id.as_deref() == input.arguments["guestId"].as_str()
            })
            .map(|l| l.view.target.device_id.clone())
            .ok_or_else(|| state.missing_lease(&agent.session, input.arguments["guestId"].as_str()))?
    } else if let Some(plan) = &plan {
        plan.target.device_id.clone()
    } else {
        selected.as_ref().unwrap().0.clone()
    };
    let scope = scope(&agent, &device);
    // Desktop's bounded ledger deduplicates by wire requestId. Fresh reads
    // therefore need distinct internal IDs as well as distinct waiter keys.
    // These IDs are not persisted or accepted as caller status authority.
    let wire_id = wire_request_id(&input);
    let key = key(&scope, &wire_id);
    let claim = match repo
        .begin_for_method(
            if non_persistent(&input) {
                "thread/read"
            } else {
                "browser/write"
            },
            &scope,
            &input.request_id,
            &canonical,
        )
        .map_err(|_| RelayError::new("browser_unavailable"))?
    {
        BeginRequest::Forward(claim) => claim,
        BeginRequest::InProgress => return wait(relay, repo, &key, stream, false),
        BeginRequest::Completed(response) => return Ok(persisted_reply(&response.body)),
        BeginRequest::Conflict => return Err(RelayError::new("request_conflict")),
        BeginRequest::OutcomeUnknown => return Err(RelayError::new("outcome_unknown")),
        BeginRequest::CapacityExceeded(full) => {
            return Ok(Reply::capacity(full))
        }
    };
    // Every path settles the claim; only a proven refusal can release it.
    let outcome = (|| -> Result<Reply> {
        if input.tool == "browser_list" {
            let state = relay
                .state
                .lock()
                .map_err(|_| RelayError::new("browser_unavailable"))?;
            let mut leases:Vec<_>=state.leases.values().filter(|l|l.view.cutex_session_id==agent.session&&l.view.state=="active").map(|l|json!({"guestId":l.view.target.guest_id,"title":l.view.title,"url":safe_url(&l.view.url)})).collect();
            leases.sort_by_key(|v| v["guestId"].as_str().unwrap_or_default().to_owned());
            drop(state);
            let result = super::results::text_result(json!(leases));
            if serde_json::to_vec(&result).map_or(true, |v| v.len() > 64 * 1024) {
                return Err(RelayError::new("message_too_large"));
            }
            complete(repo, &claim, Status::Succeeded, None, None)?;
            return Ok(Reply::success(result));
        }
        if input.tool == "browser_release" {
            let mut state = relay
                .state
                .lock()
                .map_err(|_| RelayError::new("browser_unavailable"))?;
            let id = state
                .leases
                .values()
                .find(|l| {
                    l.view.cutex_session_id == agent.session
                        && l.view.target.guest_id.as_deref() == input.arguments["guestId"].as_str()
                })
                .map(|l| l.view.lease_id.clone())
                .ok_or_else(|| state.missing_lease(&agent.session, input.arguments["guestId"].as_str()))?;
            state::revoke_lease(&mut state, &id, "lease_revoked");
            relay.changed.notify_all();
            drop(state);
            complete(repo, &claim, Status::Succeeded, None, None)?;
            return Ok(Reply::success(super::results::text_result(
                json!({"released":true}),
            )));
        }
        let (device, host, generation) = if let Some(plan) = &plan {
            (
                plan.target.device_id.clone(),
                plan.target.host_id.clone(),
                plan.generation.clone(),
            )
        } else {
            selected.clone().unwrap()
        };
        {
            let state = relay
                .state
                .lock()
                .map_err(|_| RelayError::new("browser_unavailable"))?;
            if !state
                .devices
                .get(&device)
                .is_some_and(|d| d.generation == generation && d.sender.is_some())
            {
                return Err(RelayError::new("stale_connection"));
            }
        }
        relay.check_device(&device, &generation)?;
        let owner_agent = super::owner::agent(context, &agent.session)?;
        let store = crate::approvals::ApprovalStore::open_default()
            .map_err(|_| RelayError::new("browser_unavailable"))?;
        let approval = super::approval::gate(
            &store,
            &super::approval::Context {
                agent: &agent,
                agent_name: &owner_agent.name,
                host: &host,
                device: &device,
                generation: &generation,
                full_access: owner_agent.full_access,
            },
            &input,
            crate::config::browser::current().agent_open_approval,
        )?;
        if let super::approval::Decision::Refused { code, approval_id } = approval {
            // The desktop grants local pages by itself within about a second:
            // answer with that lease instead of a round trip through the owner.
            if code == "approval_required" && input.tool == "browser_claim" {
                if let Some(lease) = relay.await_claim(&agent.session, &approval_id, CLAIM_GRANT_WAIT) {
                    complete(repo, &claim, Status::Succeeded, None, None)?;
                    return Ok(Reply::success(claimed_result(&lease, &approval_id)));
                }
            }
            return immediate_error(repo, &claim, RelayError::new(code), Some(&approval_id));
        }
        let mut plan = plan.ok_or(RelayError::new("unsupported_operation"))?;
        normalize_operation(&mut plan)?;
        let expected_identity = agent.clone();
        let identity_request = SimpleHttpRequest {
            method: "POST".into(),
            path: "/v2/browser/commands".into(),
            headers: request.headers.clone(),
            body: Vec::new(),
        };
        let identity_current = Arc::new(move || {
            auth::agent(&identity_request, context, false).is_ok_and(|now| {
                now.session == expected_identity.session
                    && now.runtime == expected_identity.runtime
                    && now.thread == expected_identity.thread
                    && now.generation == expected_identity.generation
            }) && crate::agent_bus::tool_policy::browser_tools_enabled(&expected_identity.session)
        });
        let expected = agent.clone();
        let original = SimpleHttpRequest {
            method: "POST".into(),
            path: "/v2/browser/commands".into(),
            headers: request.headers.clone(),
            body: Vec::new(),
        };
        let current = Arc::new(move || {
            auth::agent(&original, context, false).is_ok_and(|now| {
                now.session == expected.session
                    && now.runtime == expected.runtime
                    && now.thread == expected.thread
                    && now.generation == expected.generation
                    && now.turn == expected.turn
            }) && crate::agent_bus::tool_policy::browser_tools_enabled(&expected.session)
        });
        let session = agent.session.clone();
        let admission = Admission {
            key: key.clone(),
            request_id: wire_id,
            digest,
            authority: Authority {
                agent,
                current,
                identity_current,
            },
            plan,
            title: input.arguments["title"].as_str().unwrap().into(),
            deadline: started + COMMAND_TIMEOUT,
            purpose: "agent",
            claim: Some(claim.clone()),
        };
        if let super::approval::Decision::Approved { approval_id } = approval {
            store
                .try_consume_browser_open(&approval_id, &session, chrono::Utc::now(), || {
                    relay.enqueue(admission)
                })
                .map_err(|_| RelayError::new("browser_unavailable"))?
                .ok_or(RelayError::new("approval_denied"))??;
            if let Ok(Some(record)) = store.get(&approval_id, chrono::Utc::now()) {
                crate::management::v2::approvals::publish_changed(&record, "consumed");
            }
        } else {
            relay.enqueue(admission)?;
        }
        let reply = wait(relay, repo, &key, stream, true);
        if let Ok(receipt) = relay.receipt(&key) {
            let _ = settle_admission(repo, &claim, receipt);
        }
        reply
    })();
    match outcome {
        Ok(reply) => Ok(reply),
        Err(error) => {
            if relay.receipt(&key).is_ok() {
                relay.cancel(&key, error.code)?;
                let receipt = relay.receipt(&key)?;
                let _ = settle_admission(repo, &claim, receipt.clone());
                Ok(Reply::error(receipt.code.unwrap_or(error), None))
            } else {
                immediate_error(repo, &claim, error, None)
            }
        }
    }
}
/// How long browser_claim waits for the desktop's own grant of the pending
/// approval before answering approval_required.
pub const CLAIM_GRANT_WAIT: Duration = Duration::from_secs(3);
fn claimed_result(lease: &LeaseView, approval_id: &str) -> Value {
    super::results::text_result(json!({
        "claimed": true,
        "guestId": lease.target.guest_id,
        "title": lease.title,
        "url": safe_url(&lease.url),
        "approvalId": approval_id,
        "sharesOwnerLogin": true,
    }))
}
fn normalize_operation(plan: &mut Plan) -> Result<()> {
    for path in ["/url", "/action/condition/url"] {
        let Some(url) = plan.operation.pointer(path).and_then(Value::as_str) else {
            continue;
        };
        let Some(rest) = url.strip_prefix("cutex:service/") else {
            continue;
        };
        let (slug, path_part) = rest.split_once('/').unwrap_or((rest, ""));
        crate::services::store::validate_slug(slug)
            .map_err(|_| RelayError::new("invalid_request"))?;
        let mapped = format!("http://{slug}.svc.localhost/{path_part}");
        let parsed = url::Url::parse(&mapped).map_err(|_| RelayError::new("invalid_request"))?;
        if parsed.host_str() != Some(format!("{slug}.svc.localhost").as_str())
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(RelayError::new("invalid_request"));
        }
        // Only desktop main knows the live local/SSH/dev-direct connection
        // port. Validate the reference, but preserve it byte-for-byte.
    }
    Ok(())
}
fn read_only(input: &AgentCommand) -> bool {
    matches!(
        input.tool.as_str(),
        "browser_list" | "browser_read" | "browser_find" | "browser_screenshot"
    ) || input.tool == "browser_act"
        && input
            .arguments
            .pointer("/action/kind")
            .and_then(Value::as_str)
            == Some("wait")
}
fn non_persistent(input: &AgentCommand) -> bool {
    read_only(input) || input.tool == "browser_claim"
}
fn wire_request_id(input: &AgentCommand) -> String {
    if read_only(input) {
        format!("browser-read-{}", uuid::Uuid::new_v4().simple())
    } else {
        input.request_id.clone()
    }
}
fn wait(
    relay: &Arc<Relay>,
    repo: &RequestIdempotencyRepository,
    key: &str,
    stream: &TcpStream,
    owns_admission: bool,
) -> Result<Reply> {
    let admission_wait = Instant::now() + Duration::from_secs(30);
    loop {
        let receipt = match relay.receipt(key) {
            Ok(receipt) => receipt,
            Err(_) => {
                let (scope, id) = key
                    .split_once('\0')
                    .ok_or(RelayError::new("stale_request"))?;
                let prefix = scope.get(..73).ok_or(RelayError::new("stale_request"))?;
                let records = repo
                    .browser_lookup(prefix, id)
                    .map_err(|_| RelayError::new("browser_unavailable"))?;
                if let Some(metadata) = records.first().and_then(|r| r.metadata.as_ref()) {
                    return Ok(persisted_reply(metadata));
                }
                if records.first().is_none_or(|r| r.state != "forwarded")
                    || Instant::now() >= admission_wait
                {
                    return Err(RelayError::new("outcome_unknown"));
                }
                let state = relay
                    .state
                    .lock()
                    .map_err(|_| RelayError::new("browser_unavailable"))?;
                let _ = relay
                    .changed
                    .wait_timeout(state, Duration::from_millis(100))
                    .map_err(|_| RelayError::new("browser_unavailable"))?;
                continue;
            }
        };
        if receipt.status.terminal() {
            // Claim clones settle once, so a late duplicate cannot modify a
            // replacement reservation after a deterministic refusal.
            let _ = settle_receipt(repo, &receipt);
            if let Some(content) = receipt.content {
                return Ok(Reply {
                    status: 200,
                    value: Arc::clone(&content.value),
                    delivered: Some(key.into()),
                    retained: Some(content),
                });
            }
            return Ok(Reply::error(
                receipt
                    .code
                    .unwrap_or(RelayError::new("request_result_not_retained")),
                None,
            ));
        }
        // MCP uses one request/connection. A closed caller cancels only this
        // request; it never revokes the owner's underlying grant.
        stream
            .set_nonblocking(true)
            .map_err(|_| RelayError::new("browser_unavailable"))?;
        let mut byte = [0];
        let peek = stream.peek(&mut byte);
        let _ = stream.set_nonblocking(false);
        if matches!(peek, Ok(0)) || peek.is_err_and(|e| e.kind() != std::io::ErrorKind::WouldBlock)
        {
            if !owns_admission {
                return Err(RelayError::new("cancelled"));
            }
            relay.cancel(key, "cancelled")?;
            continue;
        }
        let state = relay
            .state
            .lock()
            .map_err(|_| RelayError::new("browser_unavailable"))?;
        let _ = relay
            .changed
            .wait_timeout(state, Duration::from_millis(100))
            .map_err(|_| RelayError::new("browser_unavailable"))?;
    }
}
pub fn status(
    relay: &Arc<Relay>,
    request: &SimpleHttpRequest,
    context: ManagementRequestContext,
    id: &str,
    cancel: bool,
) -> Result<Reply> {
    let agent = auth::agent(request, context, false)?;
    let repo = repository()?;
    let old = repo
        .browser_lookup(&prefix(&agent), id)
        .map_err(|_| RelayError::new("browser_unavailable"))?;
    if old.len() != 1 {
        return Err(RelayError::new("permission_denied"));
    }
    let old = &old[0];
    let key = key(&old.scope, id);
    if cancel {
        if relay.receipt(&key).is_ok() {
            relay.cancel(&key, "cancelled")?;
        }
        return Ok(Reply::success(json!({"cancelled":true})));
    }
    if let Ok(value) = relay.metadata(&key) {
        return Ok(Reply::success(value));
    }
    Ok(Reply::success(old.metadata.clone().unwrap_or_else(
        || json!({"requestId":id,"status":"outcome_unknown","resultRetained":false}),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn browser_duplicate_waiter_disconnect_does_not_cancel_original_dispatched_write() {
        use super::super::state::{Binding, DesktopIdentity};
        let relay = Arc::new(Relay::new());
        let identity = DesktopIdentity {
            id: "root:test".into(),
            valid: Arc::new(|| true),
        };
        let registration = relay
            .register(
                identity.clone(),
                Registration {
                    device_id: "d".into(),
                    host_id: "local".into(),
                    provider_id: "browser".into(),
                    executor: "inApp".into(),
                },
                10000,
                30000,
            )
            .unwrap();
        let binding = Binding {
            identity: &identity,
            device_id: "d",
            generation: registration["connectionGeneration"].as_str().unwrap(),
            key: registration["connectionKey"].as_str().unwrap(),
        };
        let device = relay.attach(&binding, None).unwrap();
        device.receiver.recv().unwrap();
        let lease = relay
            .grant_checked(
                &binding,
                GrantRequest {
                    device_id: "d".into(),
                    guest_id: "g".into(),
                    cutex_session_id: "cutex.s".into(),
                    title: "Page".into(),
                    url: "about:blank".into(),
                    source: GrantSource::Owner {},
                },
                "Agent".into(),
            )
            .unwrap();
        relay.activate(&binding, &lease.lease_id).unwrap();
        let agent = auth::AgentContext {
            session: "cutex.s".into(),
            runtime: "runtime".into(),
            thread: "thread".into(),
            generation: 1,
            turn: "turn".into(),
            credential_identity: "fixture".into(),
        };
        let root = std::env::temp_dir().join(format!("browser-waiter-{}", uuid::Uuid::new_v4()));
        let repo = RequestIdempotencyRepository::open(&root, "fixture".into()).unwrap();
        let scope = format!("{}d", prefix(&agent));
        let request_key = key(&scope, "request");
        let operation = json!({"kind":"tab.navigate","url":"http://localhost/"});
        let BeginRequest::Forward(claim) = repo.begin(&scope, "request", &operation).unwrap()
        else {
            panic!()
        };
        relay
            .enqueue(Admission {
                key: request_key.clone(),
                request_id: "request".into(),
                digest: "digest".into(),
                authority: Authority {
                    agent,
                    current: Arc::new(|| true),
                    identity_current: Arc::new(|| true),
                },
                plan: Plan {
                    target: lease.target,
                    generation: binding.generation.into(),
                    lease_id: Some(lease.lease_id),
                    operation,
                },
                title: "Navigate".into(),
                deadline: Instant::now() + COMMAND_TIMEOUT,
                purpose: "agent",
                claim: Some(claim),
            })
            .unwrap();
        assert!(!relay.receipt(&request_key).unwrap().dispatched);
        let marker = device.receiver.recv().unwrap();
        relay.prepare_frame(&binding, &marker).unwrap().unwrap();
        assert!(relay.receipt(&request_key).unwrap().dispatched);
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let caller = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        drop(caller);
        assert_eq!(
            wait(&relay, &repo, &request_key, &server, false)
                .err()
                .unwrap()
                .code,
            "cancelled"
        );
        assert_eq!(
            relay.receipt(&request_key).unwrap().status,
            Status::Dispatched
        );
        assert_eq!(
            wait(&relay, &repo, &request_key, &server, true)
                .unwrap()
                .value["error"]["code"],
            "outcome_unknown"
        );
        assert_eq!(
            relay.receipt(&request_key).unwrap().status,
            Status::OutcomeUnknown
        );
        drop(repo);
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn browser_read_operations_do_not_reserve_durable_rows() {
        for tool in [
            "browser_list",
            "browser_read",
            "browser_find",
            "browser_screenshot",
        ] {
            assert!(read_only(&AgentCommand {
                request_id: "r".into(),
                tool: tool.into(),
                arguments: json!({})
            }));
        }
        assert!(read_only(&AgentCommand {
            request_id: "r".into(),
            tool: "browser_act".into(),
            arguments: json!({"action":{"kind":"wait"}})
        }));
        for tool in [
            "browser_open",
            "browser_navigate",
            "browser_release",
            "browser_claim",
            "browser_act",
        ] {
            assert!(!read_only(&AgentCommand {
                request_id: "r".into(),
                tool: tool.into(),
                arguments: json!({"action":{"kind":"click"}})
            }));
        }
    }
    #[test]
    fn browser_claim_polling_and_pre_admission_refusals_do_not_consume_write_quota() {
        let dir = std::env::temp_dir().join(format!("browser-refusal-{}", uuid::Uuid::new_v4()));
        let repo = RequestIdempotencyRepository::open_with_policy(&dir, "fixture".into(), request_idempotency::RetentionPolicy { max_records: 1, ..Default::default() }).unwrap();
        let prefix = format!("browser:{}:", "a".repeat(64));
        let scope = format!("{prefix}device");
        let claim = AgentCommand { request_id: "claim".into(), tool: "browser_claim".into(), arguments: json!({}) };
        assert!(non_persistent(&claim));
        assert!(!read_only(&claim));
        // Reads and approval intentions bypass even an already-full quota.
        let BeginRequest::Forward(protected) = repo.begin(&scope, "protected", &json!({})).unwrap() else { panic!(); };
        repo.mark_outcome_unknown(&protected).unwrap();
        for n in 0..100 {
            let id = format!("claim-{n}");
            let BeginRequest::Forward(ephemeral) = repo.begin_for_method(if non_persistent(&claim) { "thread/read" } else { "browser/write" }, &scope, &id, &json!({})).unwrap() else { panic!(); };
            assert_eq!(immediate_error(&repo, &ephemeral, RelayError::new("approval_required"), Some("apr-fixture")).unwrap().value["error"]["details"]["approvalId"], "apr-fixture");
            assert!(repo.browser_lookup(&prefix, &id).unwrap().is_empty());
        }
        assert_eq!(repo.browser_lookup(&prefix, "protected").unwrap()[0].state, "outcome_unknown");
        drop(repo);
        let _ = std::fs::remove_dir_all(dir);

        let dir = std::env::temp_dir().join(format!("browser-refusal-{}", uuid::Uuid::new_v4()));
        let repo = RequestIdempotencyRepository::open_with_policy(&dir, "fixture".into(), request_idempotency::RetentionPolicy { max_records: 1, ..Default::default() }).unwrap();
        for code in ["approval_required", "approval_denied", "permission_denied", "busy"] {
            let BeginRequest::Forward(claim) = repo.begin(&scope, code, &json!({})).unwrap() else { panic!(); };
            immediate_error(&repo, &claim, RelayError::new(code), None).unwrap();
            assert!(repo.browser_lookup(&prefix, code).unwrap().is_empty());
        }
        drop(repo);
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn browser_deterministic_refusal_classification_is_operation_and_dispatch_specific() {
        let mut receipt = Receipt { key: "k".into(), status: Status::Failed, dispatched: true,
            operation_kind: "tab.act".into(), code: None, content: None, claim: None };
        for kind in ["tab.act", "tab.navigate", "tabs.open"] {
            receipt.operation_kind = kind.into();
            for code in ["permission_denied", "target_hidden", "owner_paused", "invalid_request", "browser_identity_unavailable", "unsupported_operation", "scope_invalid", "scope_unsupported", "navigation_failed", "service_auth_failed", "service_unavailable", "deadline_exceeded", "cancelled", "outcome_unknown"] {
                receipt.code = Some(RelayError::new(code));
                assert_eq!(deterministic_refusal(&receipt),
                    code == "unsupported_operation" || kind == "tab.act" && matches!(code, "permission_denied" | "target_hidden" | "owner_paused" | "invalid_request")
                    || kind == "tabs.open" && code == "browser_identity_unavailable", "{kind}:{code}");
                receipt.status = Status::OutcomeUnknown;
                assert!(!deterministic_refusal(&receipt), "unknown {kind}:{code}");
                receipt.status = Status::Failed;
            }
        }
        receipt.dispatched = false;
        receipt.code = Some(RelayError::new("deadline_exceeded"));
        assert!(deterministic_refusal(&receipt));
        receipt.status = Status::Cancelled;
        assert!(deterministic_refusal(&receipt));
        receipt.status = Status::Succeeded;
        assert!(!deterministic_refusal(&receipt));
    }
    #[test]
    fn browser_capacity_reply_preserves_actual_retry_and_counts() {
        let reply = Reply::capacity(request_idempotency::CapacityExceeded { max_records: 256, unresolved_records: 240, in_flight_records: 8, retry_after_ms: 600_000 });
        assert_eq!(reply.status, 503);
        assert_eq!(reply.value["error"]["details"]["retryAfterMs"], 600_000);
        assert_eq!(reply.value["error"]["details"]["maxRecords"], 256);
        assert_eq!(reply.value["error"]["retryable"], true);
    }
    #[test]
    fn browser_duplicate_terminal_refusal_settles_the_new_reservation_without_redispatch() {
        let dir = std::env::temp_dir().join(format!("browser-replay-refusal-{}", uuid::Uuid::new_v4()));
        let repo = RequestIdempotencyRepository::open(&dir, "fixture".into()).unwrap();
        let prefix = format!("browser:{}:", "a".repeat(64));
        let scope = format!("{prefix}device");
        let BeginRequest::Forward(old) = repo.begin(&scope, "r", &json!({})).unwrap() else { panic!(); };
        let old_receipt = Receipt { key: "k".into(), status: Status::Failed, dispatched: true,
            operation_kind: "tab.act".into(), code: Some(RelayError::new("target_hidden")), content: None, claim: Some(old) };
        settle_receipt(&repo, &old_receipt).unwrap();
        let BeginRequest::Forward(new) = repo.begin(&scope, "r", &json!({})).unwrap() else { panic!(); };
        settle_admission(&repo, &new, old_receipt).unwrap();
        assert!(repo.browser_lookup(&prefix, "r").unwrap().is_empty());
        drop(repo);
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn browser_same_id_reads_have_distinct_desktop_ledger_ids_but_writes_keep_replay_id() {
        let mut input = AgentCommand {
            request_id: "caller-id".into(),
            tool: "browser_read".into(),
            arguments: json!({}),
        };
        let first = wire_request_id(&input);
        let second = wire_request_id(&input);
        assert_ne!(first, second);
        assert_ne!(first, input.request_id);
        assert!(identifier(&first) && identifier(&second));
        input.tool = "browser_act".into();
        input.arguments = json!({"action":{"kind":"click"}});
        assert_eq!(wire_request_id(&input), input.request_id);
        assert_eq!(wire_request_id(&input), wire_request_id(&input));
    }
    #[test]
    fn browser_scope_is_credential_session_bound_and_lookup_prefix_safe() {
        let a = auth::AgentContext {
            session: "cutex.a".into(),
            runtime: "r".into(),
            thread: "t".into(),
            generation: 1,
            turn: "turn".into(),
            credential_identity: "agent:cred".into(),
        };
        let mut b = a.clone();
        b.session = "cutex.b".into();
        assert_ne!(prefix(&a), prefix(&b));
        assert_eq!(prefix(&a).len(), 73);
        b = a.clone();
        b.credential_identity = "replacement".into();
        assert_ne!(prefix(&a), prefix(&b));
        assert_eq!(&scope(&a, "a")[73..146], &scope(&b, "b")[73..146]);
        assert_ne!(key("a:b", "c"), key("a", "b:c"));
    }
    #[test]
    fn browser_service_mapping_cannot_change_authority() {
        let mut plan = Plan {
            target: Target {
                host_id: "local".into(),
                device_id: "d".into(),
                guest_id: None,
            },
            generation: "g".into(),
            lease_id: None,
            operation: json!({"kind":"tabs.open","url":"cutex:service/web/path?q=1"}),
        };
        normalize_operation(&mut plan).unwrap();
        assert_eq!(plan.operation["url"], "cutex:service/web/path?q=1");
        for host in ["local", "ssh-test", "dev-browser-i2"] {
            plan.target.host_id = host.into();
            normalize_operation(&mut plan).unwrap();
            assert_eq!(plan.operation["url"], "cutex:service/web/path?q=1");
        }
        for url in [
            "cutex:service/evil@host/path",
            "cutex:service/web:80/path",
            "cutex:service/../secret",
        ] {
            plan.operation["url"] = json!(url);
            assert!(normalize_operation(&mut plan).is_err());
        }
    }
    #[test]
    fn browser_wait_accepts_and_preserves_service_reference_through_admission_validation() {
        let input = AgentCommand { request_id: "wait".into(), tool: "browser_act".into(), arguments: json!({
            "title":"Wait for service", "guestId":"g", "action":{"kind":"wait","condition":{"kind":"url","url":"cutex:service/web/path?q=1"}}
        }) };
        input.validate().unwrap();
        let mut plan = Plan { target: Target { host_id: "ssh-alias".into(), device_id: "d".into(), guest_id: Some("g".into()) }, generation: "generation".into(), lease_id: Some("lease".into()), operation: json!({"kind":"tab.act","action":input.arguments["action"]}) };
        normalize_operation(&mut plan).unwrap();
        assert_eq!(plan.operation["action"]["condition"]["url"], "cutex:service/web/path?q=1");
        plan.operation["action"]["condition"]["url"] = json!("cutex:service/evil@host/path");
        assert!(normalize_operation(&mut plan).is_err());
    }
}
