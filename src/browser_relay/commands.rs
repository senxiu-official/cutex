//! Bounded, monotonic device command lifecycle. This module never opens a
//! browser or creates authority: callers must supply a current agent fence and
//! a durable idempotency claim before admission.
use super::{
    auth::AgentContext,
    model::*,
    results::Projection,
    state::{self, Binding, Relay, State},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

const MAX_METADATA: usize = 2048;
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const DISPATCH_FLOOR: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Accepted,
    Dispatched,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    OutcomeUnknown,
}
impl Status {
    pub fn terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::OutcomeUnknown
        )
    }
    pub fn sent(self) -> bool {
        matches!(self, Self::Dispatched | Self::Running)
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentCommand {
    pub request_id: String,
    pub tool: String,
    pub arguments: Value,
}
impl AgentCommand {
    pub fn validate(&self) -> Result<()> {
        if !identifier(&self.request_id) {
            return Err(RelayError::new("invalid_request"));
        }
        crate::agent_bus::mcp_browser::validate_arguments(&self.tool, &self.arguments)
            .map_err(RelayError::new)
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceError {
    pub code: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeviceResult {
    pub dispatch_seq: u64,
    pub target: Target,
    pub operation_kind: String,
    pub status: Status,
    pub metadata_only: Option<bool>,
    pub result: Option<Value>,
    pub error: Option<DeviceError>,
}
impl DeviceResult {
    pub fn parse(body: &[u8]) -> Result<Self> {
        let value: Value = super::model::parse(body)?;
        if !value["target"]
            .as_object()
            .is_some_and(|o| o.contains_key("guestId"))
            || ["metadataOnly", "result", "error"]
                .iter()
                .any(|key| value.get(*key).is_some_and(Value::is_null))
        {
            return Err(RelayError::new("invalid_result"));
        }
        let result: Self =
            serde_json::from_value(value).map_err(|_| RelayError::new("invalid_result"))?;
        result.validate()?;
        Ok(result)
    }
    pub fn validate(&self) -> Result<()> {
        let invalid = || RelayError::new("invalid_result");
        if self.dispatch_seq == 0
            || self.dispatch_seq > MAX_SEQUENCE
            || !identifier(&self.target.host_id)
            || !identifier(&self.target.device_id)
            || self
                .target
                .guest_id
                .as_ref()
                .is_some_and(|id| !identifier(id))
            || self.target.guest_id.is_none() && self.operation_kind != "tabs.open"
        {
            return Err(invalid());
        }
        if self.metadata_only == Some(false) {
            return Err(invalid());
        }
        if !self.status.terminal() {
            if self.metadata_only.is_some() || self.result.is_some() || self.error.is_some() {
                return Err(invalid());
            }
        } else if self.metadata_only == Some(true) {
            if self.result.is_some() {
                return Err(invalid());
            }
        } else if self.status == Status::Succeeded {
            if self.result.is_none() || self.error.is_some() {
                return Err(invalid());
            }
        } else if self.result.is_some() || self.error.is_none() {
            return Err(invalid());
        }
        if self.error.as_ref().is_some_and(|e| {
            e.code.len() > 64 || !e.code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
        }) {
            return Err(invalid());
        }
        Ok(())
    }
}
#[derive(Clone)]
pub struct Authority {
    pub agent: AgentContext,
    // Holds only trusted credential/fence data, never model text or page data.
    pub current: Arc<dyn Fn() -> bool + Send + Sync>,
    // The same credential/runtime fence without active-turn equality. Only the
    // trusted completion hook may use this for an owner-only final capture.
    pub identity_current: Arc<dyn Fn() -> bool + Send + Sync>,
}
#[derive(Clone)]
pub struct Plan {
    pub target: Target,
    pub generation: String,
    pub lease_id: Option<String>,
    pub operation: Value,
}
pub struct Admission {
    pub key: String,
    pub request_id: String,
    pub digest: String,
    pub authority: Authority,
    pub plan: Plan,
    pub title: String,
    pub deadline: Instant,
    pub purpose: &'static str,
    pub claim: Option<crate::management::v2::request_idempotency::RequestClaim>,
}
pub(crate) struct Cached {
    pub value: Arc<super::retained::Body>,
    pub at: Instant,
    pub bytes: usize,
    pub page_generation: Option<u64>,
    pub refs: Vec<String>,
}
pub(crate) struct Command {
    pub key: String,
    pub request_id: String,
    pub digest: String,
    pub authority: Authority,
    pub target: Target,
    pub generation: String,
    pub lease_id: Option<String>,
    pub operation: Value,
    pub operation_kind: String,
    pub title: String,
    pub purpose: &'static str,
    pub deadline: Instant,
    pub sequence: u64,
    pub status: Status,
    pub mutating: bool,
    pub dispatched: bool,
    pub ended: Option<Instant>,
    pub code: Option<RelayError>,
    pub body: Option<Cached>,
    pub claim: Option<crate::management::v2::request_idempotency::RequestClaim>,
    pub late_results: u64,
    pub open_lease: Option<String>,
    pub started: Instant,
    retention_limit: usize,
}
#[derive(Clone)]
pub struct Receipt {
    pub key: String,
    pub status: Status,
    pub dispatched: bool,
    pub operation_kind: String,
    pub code: Option<RelayError>,
    pub content: Option<Arc<super::retained::Body>>,
    pub claim: Option<crate::management::v2::request_idempotency::RequestClaim>,
}
pub fn has_side_effects(operation: &Value) -> bool {
    match operation["kind"].as_str() {
        Some("tab.snapshot" | "tab.find" | "tab.screenshot" | "tabs.list") => false,
        Some("tab.act") => operation["action"]["kind"] != "wait",
        _ => true,
    }
}
fn operation(input: &AgentCommand) -> Result<Value> {
    Ok(match input.tool.as_str() {
        "browser_read" => json!({"kind":"tab.snapshot"}),
        "browser_find" => json!({"kind":"tab.find","query":input.arguments["query"]}),
        "browser_act" => json!({"kind":"tab.act","action":input.arguments["action"]}),
        "browser_navigate" => json!({"kind":"tab.navigate","url":input.arguments["url"]}),
        "browser_screenshot" => {
            let mut value = json!({"kind":"tab.screenshot"});
            if let Some(r) = input.arguments.get("ref") {
                value["ref"] = r.clone();
            }
            value
        }
        "browser_open" => json!({"kind":"tabs.open","url":input.arguments["url"]}),
        _ => return Err(RelayError::new("unsupported_operation")),
    })
}
impl Relay {
    pub fn plan(&self, agent: &AgentContext, input: &AgentCommand) -> Result<Plan> {
        input.validate()?;
        let operation = operation(input)?;
        let state = self
            .state
            .lock()
            .map_err(|_| RelayError::new("browser_unavailable"))?;
        if input.tool == "browser_open" {
            let device =
                state::preferred_device(&state).ok_or(RelayError::new("browser_unavailable"))?;
            let identity = device.identity.clone();
            let plan = Plan {
                target: Target {
                    host_id: device.registration.host_id.clone(),
                    device_id: device.registration.device_id.clone(),
                    guest_id: None,
                },
                generation: device.generation.clone(),
                lease_id: None,
                operation,
            };
            drop(state);
            identity.check()?;
            return Ok(plan);
        }
        let guest = input.arguments["guestId"]
            .as_str()
            .ok_or(RelayError::new("invalid_request"))?;
        let lease = state.leases.values().find(|l| {
            l.view.cutex_session_id == agent.session
                && l.view.state == "active"
                && l.view.target.guest_id.as_deref() == Some(guest)
        });
        if !state.devices.values().any(|d| d.sender.is_some()) {
            // A lease the desktop's restart ended says so even before it is back.
            let ended = state.missing_lease(&agent.session, Some(guest));
            return Err(if lease.is_none() && ended.code == state::LEASE_ENDED_DESKTOP_RESTARTED {
                ended
            } else {
                RelayError::new("browser_unavailable")
            });
        }
        let lease = lease.ok_or_else(|| state.missing_lease(&agent.session, Some(guest)))?;
        let device = state
            .devices
            .get(&lease.view.target.device_id)
            .filter(|d| d.sender.is_some() && d.generation == lease.generation)
            .ok_or(RelayError::new("browser_unavailable"))?;
        let identity = device.identity.clone();
        let plan = Plan {
            target: lease.view.target.clone(),
            generation: device.generation.clone(),
            lease_id: Some(lease.view.lease_id.clone()),
            operation,
        };
        drop(state);
        identity.check()?;
        Ok(plan)
    }
    pub fn enqueue(&self, admission: Admission) -> Result<String> {
        if !(admission.authority.current)() {
            return Err(RelayError::new("permission_denied"));
        }
        self.check_device(&admission.plan.target.device_id, &admission.plan.generation)?;
        let retention_limit = crate::config::browser::current().image_retain_bytes as usize;
        let now = Instant::now();
        if admission.deadline.saturating_duration_since(now) <= DISPATCH_FLOOR {
            return Err(RelayError::new("deadline_exceeded"));
        }
        if admission.deadline.saturating_duration_since(now) > Duration::from_secs(30)
            || !identifier(&admission.request_id)
            || !display_text(&admission.title, 200)
            || !matches!(admission.purpose, "agent" | "owner-turn-shot")
        {
            return Err(RelayError::new("invalid_request"));
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| RelayError::new("browser_unavailable"))?;
        if let Some(old) = state.requests.get(&admission.key) {
            return if old.digest == admission.digest {
                Ok(old.key.clone())
            } else {
                Err(RelayError::new("request_conflict"))
            };
        }
        prune_metadata(&mut state);
        if state.requests.len() >= MAX_METADATA
            || state
                .requests
                .values()
                .filter(|r| !r.status.terminal())
                .count()
                >= MAX_REQUESTS
            || state
                .requests
                .values()
                .filter(|r| {
                    r.target.device_id == admission.plan.target.device_id && !r.status.terminal()
                })
                .count()
                >= MAX_DEVICE_REQUESTS
        {
            return Err(RelayError::new("busy"));
        }
        let device = state
            .devices
            .get(&admission.plan.target.device_id)
            .filter(|d| d.generation == admission.plan.generation && d.sender.is_some())
            .ok_or(RelayError::new("stale_connection"))?;
        if device.registration.host_id != admission.plan.target.host_id {
            return Err(RelayError::new("permission_denied"));
        }
        if let Some(id) = &admission.plan.lease_id {
            let lease = state
                .leases
                .get(id)
                .filter(|l| {
                    l.view.state == "active"
                        && l.view.cutex_session_id == admission.authority.agent.session
                        && l.view.target == admission.plan.target
                        && l.generation == admission.plan.generation
                })
                .ok_or(RelayError::new("lease_revoked"))?;
            let reference = admission
                .plan
                .operation
                .get("ref")
                .or_else(|| admission.plan.operation.pointer("/action/ref"));
            if reference
                .and_then(Value::as_str)
                .is_some_and(|r| !lease.refs.contains(r))
            {
                return Err(RelayError::new("stale_ref"));
            }
        } else if admission.plan.operation["kind"] != "tabs.open" {
            return Err(RelayError::new("permission_denied"));
        }
        let kind = admission.plan.operation["kind"]
            .as_str()
            .ok_or(RelayError::new("invalid_request"))?
            .to_owned();
        let device = state
            .devices
            .get_mut(&admission.plan.target.device_id)
            .unwrap();
        if device.sequence >= MAX_SEQUENCE {
            return Err(RelayError::new("stale_connection"));
        }
        device.sequence += 1;
        let sequence = device.sequence;
        // The queue contains only an internal marker. The actual frame and
        // remaining time are built immediately before the SSE write.
        let marker = json!({"requestId":admission.request_id,"dispatchSeq":sequence});
        device
            .sender
            .as_ref()
            .unwrap()
            .try_send(marker)
            .map_err(|_| RelayError::new("busy"))?;
        if let Some(id) = &admission.plan.lease_id {
            state.leases.get_mut(id).unwrap().touched = now;
        }
        let key = admission.key.clone();
        let mutating = has_side_effects(&admission.plan.operation);
        state.requests.insert(
            key.clone(),
            Command {
                key: key.clone(),
                request_id: admission.request_id,
                digest: admission.digest,
                authority: admission.authority,
                target: admission.plan.target,
                generation: admission.plan.generation,
                lease_id: admission.plan.lease_id,
                operation: admission.plan.operation,
                operation_kind: kind,
                title: admission.title,
                purpose: admission.purpose,
                deadline: admission.deadline,
                sequence,
                status: Status::Accepted,
                mutating,
                dispatched: false,
                ended: None,
                code: None,
                body: None,
                claim: admission.claim,
                late_results: 0,
                open_lease: None,
                started: now,
                retention_limit,
            },
        );
        self.changed.notify_all();
        Ok(key)
    }
    pub fn prepare_frame(&self, binding: &Binding<'_>, marker: &Value) -> Result<Option<Value>> {
        binding.identity.check()?;
        let (key, current) = {
            let state = self
                .state
                .lock()
                .map_err(|_| RelayError::new("browser_unavailable"))?;
            state::validate_binding(&state, binding)?;
            let request = state
                .requests
                .values()
                .find(|r| {
                    r.target.device_id == binding.device_id
                        && r.generation == binding.generation
                        && marker["requestId"] == r.request_id
                        && marker["dispatchSeq"] == r.sequence
                })
                .ok_or(RelayError::new("stale_request"))?;
            if request.status != Status::Accepted {
                return Ok(None);
            }
            (request.key.clone(), Arc::clone(&request.authority.current))
        };
        let authorized = current();
        binding.identity.check()?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| RelayError::new("browser_unavailable"))?;
        state::validate_binding(&state, binding)?;
        let request = state
            .requests
            .get_mut(&key)
            .ok_or(RelayError::new("stale_request"))?;
        if request.status != Status::Accepted {
            return Ok(None);
        }
        if !authorized {
            finish(
                request,
                Status::Failed,
                Some(RelayError::new("permission_denied")),
                None,
            );
            self.changed.notify_all();
            return Ok(None);
        }
        let remaining = request.deadline.saturating_duration_since(Instant::now());
        if remaining <= DISPATCH_FLOOR {
            finish(
                request,
                Status::Failed,
                Some(RelayError::new("deadline_exceeded")),
                None,
            );
            self.changed.notify_all();
            return Ok(None);
        }
        request.status = Status::Dispatched;
        request.dispatched = true;
        let mut fields = json!({"requestId":request.request_id,"dispatchSeq":request.sequence,"remainingMs":remaining.as_millis() as u64,
            "cutexSessionId":request.authority.agent.session,"turnId":request.authority.agent.turn,"target":request.target,"title":request.title,"operation":request.operation,"purpose":request.purpose});
        if let Some(id) = &request.lease_id {
            fields["leaseId"] = json!(id);
        }
        super::shots::mark_dispatched(&mut state, &key);
        let device = state.devices.get(binding.device_id).unwrap();
        Ok(Some(state::event(device, "command", fields)))
    }
    pub fn accept_result(
        &self,
        binding: &Binding<'_>,
        request_id: &str,
        result: DeviceResult,
    ) -> Result<bool> {
        result.validate()?;
        binding.identity.check()?;
        let settings = crate::config::browser::current();
        let (key, kind, current) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| RelayError::new("browser_unavailable"))?;
            state::validate_binding(&state, binding)?;
            let request = state
                .requests
                .values_mut()
                .find(|r| {
                    r.target.device_id == binding.device_id
                        && r.generation == binding.generation
                        && r.request_id == request_id
                        && r.sequence == result.dispatch_seq
                })
                .ok_or(RelayError::new("stale_request"))?;
            if request.sequence != result.dispatch_seq
                || request.target != result.target
                || request.operation_kind != result.operation_kind
            {
                return Err(RelayError::new("invalid_result"));
            }
            if request.status.terminal() {
                request.late_results = request.late_results.saturating_add(1);
                audit(request);
                return Ok(true);
            }
            if Instant::now() >= request.deadline {
                let key = request.key.clone();
                request.late_results = request.late_results.saturating_add(1);
                cancel_locked(&mut state, &key, "deadline_exceeded");
                self.changed.notify_all();
                return Ok(true);
            }
            if request.status == Status::Accepted {
                return Err(RelayError::new("stale_request"));
            }
            if !result.status.terminal() {
                if result.status == Status::Running {
                    request.status = Status::Running;
                }
                return Ok(false);
            }
            (
                request.key.clone(),
                request.operation_kind.clone(),
                Arc::clone(&request.authority.current),
            )
        };
        let authorized = current();
        let projection = if result.status == Status::Succeeded && result.metadata_only != Some(true)
        {
            Some(super::results::project(result.result.unwrap(), &kind))
        } else {
            None
        };
        binding.identity.check()?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| RelayError::new("browser_unavailable"))?;
        state::validate_binding(&state, binding)?;
        let request = state
            .requests
            .get(&key)
            .ok_or(RelayError::new("stale_request"))?;
        if request.status.terminal() {
            return Ok(true);
        }
        if Instant::now() >= request.deadline || !authorized {
            cancel_locked(
                &mut state,
                &key,
                if authorized {
                    "deadline_exceeded"
                } else {
                    "permission_denied"
                },
            );
            self.changed.notify_all();
            return Ok(true);
        }
        let mutating = request.mutating;
        let mut status = result.status;
        let mut code = result.error.as_ref().map(|e| RelayError::provider(&e.code));
        let mut valid_projection = None;
        if let Some(projected) = projection {
            match projected {
                Ok(projected) => {
                    let lease_id = request
                        .lease_id
                        .as_ref()
                        .or(request.open_lease.as_ref())
                        .cloned();
                    if kind == "tabs.open" {
                        let matches = projected
                            .opened
                            .as_ref()
                            .zip(lease_id.as_ref().and_then(|id| state.leases.get(id)))
                            .is_some_and(|(tab, lease)| {
                                lease.view.state == "active"
                                    && lease.view.target.guest_id.as_deref() == Some(&tab.guest_id)
                                    && lease.view.cutex_session_id
                                        == request.authority.agent.session
                                    && lease.generation == request.generation
                            });
                        if !matches {
                            status = Status::OutcomeUnknown;
                            code = Some(RelayError::new("outcome_unknown"));
                        }
                    }
                    if status == Status::Succeeded {
                        if let Some(lease) = lease_id.and_then(|id| state.leases.get_mut(&id)) {
                            if projected
                                .page_generation
                                .zip(lease.page_generation)
                                .is_some_and(|(new, old)| new < old)
                            {
                                status = if mutating {
                                    Status::OutcomeUnknown
                                } else {
                                    Status::Failed
                                };
                                code = Some(RelayError::new(if mutating {
                                    "outcome_unknown"
                                } else {
                                    "stale_ref"
                                }));
                            } else {
                                if projected.page_generation != lease.page_generation {
                                    lease.refs.clear();
                                }
                                lease.page_generation = projected.page_generation;
                            }
                        }
                        if status == Status::Succeeded {
                            valid_projection = Some(projected);
                        }
                    }
                }
                Err(error) => {
                    let request = state.requests.get(&key).unwrap();
                    status = if request.mutating {
                        Status::OutcomeUnknown
                    } else {
                        Status::Failed
                    };
                    code = Some(if request.mutating {
                        RelayError::new("outcome_unknown")
                    } else {
                        error
                    });
                }
            }
        }
        if result.metadata_only == Some(true) && code.is_none() {
            code = Some(RelayError::new("request_result_not_retained"));
        }
        if let Some(projection) = &valid_projection {
            let bytes = serde_json::to_vec(&projection.content)
                .map_or(MAX_MESSAGE_BYTES + 1, |v| v.len())
                .saturating_add(
                    if state
                        .requests
                        .get(&key)
                        .is_some_and(|r| r.purpose == "owner-turn-shot")
                    {
                        projection
                            .validated_image
                            .as_ref()
                            .map_or(0, |image| image.bytes.len())
                    } else {
                        0
                    },
                );
            let max = settings.image_retain_bytes as usize;
            while super::retained::Body::total().saturating_add(bytes) > max {
                let oldest = state
                    .requests
                    .values()
                    .filter_map(|r| r.body.as_ref().map(|b| (r.key.clone(), b.at)))
                    .min_by_key(|(_, time)| *time);
                let Some((old, _)) = oldest else {
                    break;
                };
                state.requests.get_mut(&old).unwrap().body = None;
            }
        }
        let request = state.requests.get_mut(&key).unwrap();
        request.retention_limit = settings.image_retain_bytes as usize;
        finish(request, status, code, valid_projection);
        let rejected_open = if status != Status::Succeeded {
            request.open_lease.clone()
        } else {
            None
        };
        if let Some(lease) = rejected_open {
            state::revoke_lease(&mut state, &lease, "lease_revoked");
        }
        sweep_commands(
            &mut state,
            Instant::now(),
            Duration::from_secs(settings.image_retain_secs),
            settings.image_retain_bytes as usize,
        );
        self.changed.notify_all();
        Ok(false)
    }
    pub fn receipt(&self, key: &str) -> Result<Receipt> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| RelayError::new("browser_unavailable"))?;
        if state
            .requests
            .get(key)
            .is_some_and(|r| !r.status.terminal() && Instant::now() >= r.deadline)
        {
            cancel_locked(&mut state, key, "deadline_exceeded");
        }
        let request = state
            .requests
            .get(key)
            .ok_or(RelayError::new("request_result_not_retained"))?;
        Ok(Receipt {
            key: key.into(),
            status: request.status,
            dispatched: request.dispatched,
            operation_kind: request.operation_kind.clone(),
            code: request.code,
            content: request.body.as_ref().map(|b| Arc::clone(&b.value)),
            claim: request.claim.clone(),
        })
    }
    pub fn delivered(&self, key: &str) {
        if let Ok(mut state) = self.state.lock() {
            super::shots::mark_delivered(&mut state, key);
            let Some(request) = state.requests.get_mut(key) else {
                return;
            };
            let lease_id = request
                .lease_id
                .clone()
                .or_else(|| request.open_lease.clone());
            let generation = request.generation.clone();
            let replaces_refs =
                matches!(request.operation_kind.as_str(), "tab.snapshot" | "tab.find");
            if let Some(body) = request.body.take() {
                if let Some(lease) = lease_id.and_then(|id| state.leases.get_mut(&id)) {
                    if lease.generation == generation
                        && lease.page_generation == body.page_generation
                        && replaces_refs
                    {
                        lease.refs = body.refs.into_iter().collect();
                    }
                }
            }
        }
    }
    pub fn cancel(&self, key: &str, code: &'static str) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| RelayError::new("browser_unavailable"))?;
        let request = state
            .requests
            .get_mut(key)
            .ok_or(RelayError::new("stale_request"))?;
        if request.status.terminal() {
            return Ok(());
        }
        cancel_locked(&mut state, key, code);
        self.changed.notify_all();
        Ok(())
    }
    pub fn metadata(&self, key: &str) -> Result<Value> {
        let receipt = self.receipt(key)?;
        Ok(
            json!({"requestId":self.state.lock().map_err(|_|RelayError::new("browser_unavailable"))?.requests.get(key).map(|r|r.request_id.clone()).ok_or(RelayError::new("stale_request"))?,"status":receipt.status,"resultRetained":receipt.content.is_some(),"error":receipt.code.map(|e|json!({"code":e.code}))}),
        )
    }
}
fn cancel_locked(state: &mut State, key: &str, code: &'static str) {
    let Some(request) = state.requests.get_mut(key) else {
        return;
    };
    if request.status.terminal() {
        return;
    }
    let fields =
        json!({"requestId":request.request_id,"dispatchSeq":request.sequence,"reason":code});
    let device = request.target.device_id.clone();
    let sent = request.status.sent();
    let open = request.open_lease.clone();
    abort(request, code);
    if sent {
        let delivered = state.devices.get(&device).is_some_and(|d| {
            d.sender
                .as_ref()
                .is_some_and(|s| s.try_send(state::event(d, "cancel", fields)).is_ok())
        });
        if !delivered {
            state::close_device(state, &device);
        }
    }
    if let Some(id) = open {
        state::revoke_lease(state, &id, "lease_revoked");
    }
}
fn finish(
    request: &mut Command,
    status: Status,
    code: Option<RelayError>,
    projection: Option<Projection>,
) {
    if request.status.terminal() {
        return;
    }
    request.status = status;
    request.code = code;
    request.ended = Some(Instant::now());
    request.operation = Value::Null;
    request.title.clear();
    request.body = projection.and_then(|p| {
        let bytes = serde_json::to_vec(&p.content).map_or(MAX_MESSAGE_BYTES + 1, |v| v.len());
        let image = if request.purpose == "owner-turn-shot" {
            p.validated_image
        } else {
            None
        };
        let total_bytes = bytes.saturating_add(image.as_ref().map_or(0, |image| image.bytes.len()));
        let value = super::retained::Body::new_with_image(
            p.content,
            bytes,
            request.retention_limit,
            image,
        )?;
        Some(Cached {
            bytes: total_bytes,
            value,
            at: Instant::now(),
            page_generation: p.page_generation,
            refs: p.refs,
        })
    });
    audit(request);
}
fn audit(request: &Command) {
    super::audit::record(super::audit::Record {
        time: chrono::Utc::now(),
        request_id: request.request_id.clone(),
        device_id: request.target.device_id.clone(),
        generation: request.generation.clone(),
        lease_id: request
            .lease_id
            .clone()
            .or_else(|| request.open_lease.clone()),
        operation_kind: request.operation_kind.clone(),
        status: request.status,
        error: request.code.map(|e| e.code),
        duration_ms: request.started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        bytes: request.body.as_ref().map_or(0, |b| b.bytes),
        late_result: request.late_results,
        dropped: 0,
        cleanup: None,
    });
}
fn abort(request: &mut Command, code: &'static str) {
    if request.status.terminal() {
        return;
    }
    let unknown = request.status.sent() && request.mutating;
    finish(
        request,
        if unknown {
            Status::OutcomeUnknown
        } else if code == "cancelled" {
            Status::Cancelled
        } else {
            Status::Failed
        },
        Some(RelayError::new(if unknown {
            "outcome_unknown"
        } else {
            code
        })),
        None,
    );
}
pub(crate) fn abort_matching(
    state: &mut State,
    predicate: impl Fn(&Command) -> bool,
    code: &'static str,
) {
    let mut open_leases = Vec::new();
    for request in state.requests.values_mut().filter(|r| predicate(r)) {
        request.body = None;
        if !request.status.terminal() {
            if let Some(id) = &request.open_lease {
                open_leases.push(id.clone());
            }
            abort(request, code);
        }
    }
    for id in open_leases {
        state::revoke_lease(state, &id, "lease_revoked");
    }
}
fn prune_metadata(state: &mut State) {
    while state.requests.len() >= MAX_METADATA {
        let oldest = state
            .requests
            .values()
            .filter_map(|r| r.ended.map(|ended| (r.key.clone(), ended)))
            .min_by_key(|(_, time)| *time);
        if let Some((key, _)) = oldest {
            state.requests.remove(&key);
        } else {
            break;
        }
    }
}
pub(crate) fn sweep_commands(
    state: &mut State,
    now: Instant,
    retention: Duration,
    max_bytes: usize,
) {
    let expired: Vec<_> = state
        .requests
        .values()
        .filter(|r| !r.status.terminal() && now >= r.deadline)
        .map(|r| r.key.clone())
        .collect();
    for key in expired {
        cancel_locked(state, &key, "deadline_exceeded");
    }
    for request in state.requests.values_mut() {
        if request
            .body
            .as_ref()
            .is_some_and(|b| now.saturating_duration_since(b.at) >= retention)
        {
            request.body = None;
        }
    }
    let mut total: usize = state
        .requests
        .values()
        .filter_map(|r| r.body.as_ref().map(|b| b.bytes))
        .sum();
    while total > max_bytes {
        let oldest = state
            .requests
            .values()
            .filter_map(|r| r.body.as_ref().map(|b| (r.key.clone(), b.at)))
            .min_by_key(|(_, time)| *time);
        let Some((key, _)) = oldest else {
            break;
        };
        if let Some(body) = state.requests.get_mut(&key).unwrap().body.take() {
            total = total.saturating_sub(body.bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::state::{DesktopIdentity, DeviceStream};
    use super::*;
    struct Fixture {
        relay: Arc<Relay>,
        identity: DesktopIdentity,
        registration: Value,
        stream: DeviceStream,
        lease: String,
    }
    impl Fixture {
        fn new() -> Self {
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
            let stream = relay.attach(&binding, None).unwrap();
            stream.receiver.recv().unwrap();
            let lease = relay
                .grant_checked(
                    &binding,
                    GrantRequest {
                        device_id: "d".into(),
                        guest_id: "g".into(),
                        cutex_session_id: "cutex.s".into(),
                        title: "Page".into(),
                        url: "http://localhost/".into(),
                        source: GrantSource::Owner {},
                    },
                    "Agent".into(),
                )
                .unwrap();
            relay.activate(&binding, &lease.lease_id).unwrap();
            Self {
                relay,
                identity,
                registration,
                stream,
                lease: lease.lease_id,
            }
        }
        fn binding(&self) -> Binding<'_> {
            Binding {
                identity: &self.identity,
                device_id: "d",
                generation: self.registration["connectionGeneration"].as_str().unwrap(),
                key: self.registration["connectionKey"].as_str().unwrap(),
            }
        }
        fn admission(&self, id: &str, operation: Value) -> Admission {
            Admission {
                key: id.into(),
                request_id: id.into(),
                digest: crate::approvals::request_digest(&operation),
                authority: Authority {
                    agent: AgentContext {
                        session: "cutex.s".into(),
                        runtime: "stock.a".into(),
                        thread: "t".into(),
                        generation: 1,
                        turn: "turn1".into(),
                        credential_identity: "agent:a".into(),
                    },
                    current: Arc::new(|| true),
                    identity_current: Arc::new(|| true),
                },
                plan: Plan {
                    target: Target {
                        host_id: "local".into(),
                        device_id: "d".into(),
                        guest_id: Some("g".into()),
                    },
                    generation: self.registration["connectionGeneration"]
                        .as_str()
                        .unwrap()
                        .into(),
                    lease_id: Some(self.lease.clone()),
                    operation,
                },
                title: "Observe".into(),
                deadline: Instant::now() + COMMAND_TIMEOUT,
                purpose: "agent",
                claim: None,
            }
        }
        fn dispatch(&self) -> Value {
            let marker = self.stream.receiver.recv().unwrap();
            self.relay
                .prepare_frame(&self.binding(), &marker)
                .unwrap()
                .unwrap()
        }
        fn result(&self, frame: &Value, status: &str, result: Option<Value>) -> DeviceResult {
            let mut value = json!({"dispatchSeq":frame["dispatchSeq"],"target":frame["target"],"operationKind":frame["operation"]["kind"],"status":status});
            if let Some(result) = result {
                value["result"] = result;
            }
            serde_json::from_value(value).unwrap()
        }
    }
    #[test]
    fn browser_command_duplicate_does_not_redispatch_or_extend_deadline() {
        let f = Fixture::new();
        let first = f.admission("r", json!({"kind":"tab.snapshot"}));
        let deadline = first.deadline;
        f.relay.enqueue(first).unwrap();
        f.relay
            .enqueue(f.admission("r", json!({"kind":"tab.snapshot"})))
            .unwrap();
        assert_eq!(
            f.relay.state.lock().unwrap().requests["r"].deadline,
            deadline
        );
        assert_eq!(
            f.relay
                .enqueue(f.admission(
                    "r",
                    json!({"kind":"tab.navigate","url":"http://localhost/"})
                ))
                .unwrap_err()
                .code,
            "request_conflict"
        );
        let frame = f.dispatch();
        assert_eq!(frame["remainingMs"].as_u64().unwrap() <= 10000, true);
        assert!(f.stream.receiver.try_recv().is_err());
        assert!(f
            .relay
            .prepare_frame(
                &f.binding(),
                &json!({"requestId":"r","dispatchSeq":frame["dispatchSeq"]})
            )
            .unwrap()
            .is_none());
    }
    #[test]
    fn browser_command_250ms_floor_and_revoked_fence_prevent_dispatch() {
        let f = Fixture::new();
        let mut input = f.admission("r", json!({"kind":"tab.snapshot"}));
        input.deadline = Instant::now() + DISPATCH_FLOOR;
        assert_eq!(
            f.relay.enqueue(input).unwrap_err().code,
            "deadline_exceeded"
        );
        let live = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let mut input = f.admission("r2", json!({"kind":"tab.snapshot"}));
        let check = Arc::clone(&live);
        input.authority.current = Arc::new(move || check.load(std::sync::atomic::Ordering::SeqCst));
        f.relay.enqueue(input).unwrap();
        live.store(false, std::sync::atomic::Ordering::SeqCst);
        let marker = f.stream.receiver.recv().unwrap();
        assert!(f
            .relay
            .prepare_frame(&f.binding(), &marker)
            .unwrap()
            .is_none());
        assert_eq!(
            f.relay.receipt("r2").unwrap().code.unwrap().code,
            "permission_denied"
        );
    }
    #[test]
    fn browser_command_disconnect_is_unknown_only_after_side_effect_dispatch() {
        let f = Fixture::new();
        f.relay
            .enqueue(f.admission(
                "write",
                json!({"kind":"tab.navigate","url":"http://localhost/"}),
            ))
            .unwrap();
        f.dispatch();
        f.relay
            .enqueue(f.admission("read", json!({"kind":"tab.snapshot"})))
            .unwrap();
        f.dispatch();
        f.relay
            .enqueue(f.admission(
                "queued",
                json!({"kind":"tab.navigate","url":"http://localhost/"}),
            ))
            .unwrap();
        f.relay.disconnect(
            "d",
            f.registration["connectionGeneration"].as_str().unwrap(),
        );
        assert_eq!(
            f.relay.receipt("write").unwrap().status,
            Status::OutcomeUnknown
        );
        assert_eq!(f.relay.receipt("read").unwrap().status, Status::Failed);
        assert_eq!(
            f.relay.receipt("queued").unwrap().code.unwrap().code,
            "stale_connection"
        );
    }
    #[test]
    fn browser_command_model_refs_only_activate_on_successful_delivery() {
        let f = Fixture::new();
        f.relay
            .enqueue(f.admission("read", json!({"kind":"tab.snapshot"})))
            .unwrap();
        let frame = f.dispatch();
        let result=f.result(&frame,"succeeded",Some(json!({"kind":"tab.snapshot","pageGeneration":1,"snapshotId":1,"url":"http://localhost/","nodes":[{"ref":"r1","role":"button","name":"Submit"}],"truncated":false})));
        assert!(!f.relay.accept_result(&f.binding(), "read", result).unwrap());
        assert_eq!(
            f.relay
                .enqueue(f.admission(
                    "action",
                    json!({"kind":"tab.act","action":{"kind":"click","ref":"r1"}})
                ))
                .unwrap_err()
                .code,
            "stale_ref"
        );
        assert!(f.relay.receipt("read").unwrap().content.is_some());
        f.relay.delivered("read");
        assert!(f.relay.receipt("read").unwrap().content.is_none());
        f.relay
            .enqueue(f.admission(
                "action",
                json!({"kind":"tab.act","action":{"kind":"click","ref":"r1"}}),
            ))
            .unwrap();
    }
    #[test]
    fn browser_command_late_result_cannot_resurrect_and_metadata_only_does_not_replay() {
        let f = Fixture::new();
        f.relay
            .enqueue(f.admission(
                "r",
                json!({"kind":"tab.navigate","url":"http://localhost/"}),
            ))
            .unwrap();
        let frame = f.dispatch();
        f.relay.cancel("r", "cancelled").unwrap();
        let success = f.result(
            &frame,
            "succeeded",
            Some(json!({"kind":"tab.navigate","pageGeneration":1,"url":"http://localhost/"})),
        );
        assert!(f.relay.accept_result(&f.binding(), "r", success).unwrap());
        assert_eq!(f.relay.receipt("r").unwrap().status, Status::OutcomeUnknown);
        let _cancel = f.stream.receiver.recv().unwrap();
        f.relay
            .enqueue(f.admission("r2", json!({"kind":"tab.snapshot"})))
            .unwrap();
        let frame = f.dispatch();
        let mut receipt = f.result(&frame, "succeeded", None);
        receipt.metadata_only = Some(true);
        f.relay.accept_result(&f.binding(), "r2", receipt).unwrap();
        let receipt = f.relay.receipt("r2").unwrap();
        assert_eq!(receipt.status, Status::Succeeded);
        assert_eq!(receipt.code.unwrap().code, "request_result_not_retained");
        assert!(receipt.content.is_none());
    }
    #[test]
    fn browser_command_results_strict_authority_and_after_dispatch_validation() {
        let f = Fixture::new();
        f.relay
            .enqueue(f.admission(
                "r",
                json!({"kind":"tab.navigate","url":"http://localhost/"}),
            ))
            .unwrap();
        let frame = f.dispatch();
        let mut wrong = f.result(&frame, "running", None);
        wrong.target.host_id = "another".into();
        assert_eq!(
            f.relay
                .accept_result(&f.binding(), "r", wrong)
                .unwrap_err()
                .code,
            "invalid_result"
        );
        let invalid = f.result(
            &frame,
            "succeeded",
            Some(json!({"kind":"tab.navigate","pageGeneration":1,"url":"file:///secret"})),
        );
        f.relay.accept_result(&f.binding(), "r", invalid).unwrap();
        assert_eq!(f.relay.receipt("r").unwrap().status, Status::OutcomeUnknown);
        assert!(!has_side_effects(
            &json!({"kind":"tab.act","action":{"kind":"wait"}})
        ));
        assert!(serde_json::from_value::<DeviceResult>(json!({"dispatchSeq":1,"target":{"hostId":"local","deviceId":"d","guestId":"g"},"operationKind":"tab.snapshot","status":"running","leaseId":"spoof"})).is_err());
    }
    #[test]
    fn browser_command_capacity_ttl_and_budget_are_bounded() {
        let f = Fixture::new();
        for n in 0..MAX_DEVICE_REQUESTS {
            f.relay
                .enqueue(f.admission(&format!("r{n}"), json!({"kind":"tab.snapshot"})))
                .unwrap();
        }
        assert_eq!(
            f.relay
                .enqueue(f.admission("overflow", json!({"kind":"tab.snapshot"})))
                .unwrap_err()
                .code,
            "busy"
        );
        let frame = f.dispatch();
        let result=f.result(&frame,"succeeded",Some(json!({"kind":"tab.snapshot","pageGeneration":1,"snapshotId":1,"url":"http://localhost/","nodes":[],"truncated":false})));
        f.relay
            .accept_result(&f.binding(), frame["requestId"].as_str().unwrap(), result)
            .unwrap();
        let key = frame["requestId"].as_str().unwrap();
        assert!(f.relay.receipt(key).unwrap().content.is_some());
        sweep_commands(
            &mut f.relay.state.lock().unwrap(),
            Instant::now(),
            Duration::from_secs(120),
            1,
        );
        assert!(f.relay.receipt(key).unwrap().content.is_none());
    }
    #[test]
    fn browser_command_timeout_cancels_and_revokes_pending_open_lease() {
        let f = Fixture::new();
        let mut admission = f.admission("open", json!({"kind":"tabs.open","url":"about:blank"}));
        admission.plan.target.guest_id = None;
        admission.plan.lease_id = None;
        f.relay.enqueue(admission).unwrap();
        f.dispatch();
        {
            let mut state = f.relay.state.lock().unwrap();
            let request = state.requests.get_mut("open").unwrap();
            request.open_lease = Some(f.lease.clone());
            request.deadline = Instant::now() - Duration::from_millis(1);
        }
        assert_eq!(
            f.relay.receipt("open").unwrap().status,
            Status::OutcomeUnknown
        );
        assert!(!f.relay.state.lock().unwrap().leases.contains_key(&f.lease));
        let cancel = f.stream.receiver.recv().unwrap();
        assert_eq!(cancel["kind"], "cancel");
        assert_eq!(f.stream.receiver.recv().unwrap()["kind"], "lease-revoked");
    }
    #[test]
    fn browser_command_page_regression_after_write_is_unknown() {
        let f = Fixture::new();
        f.relay
            .state
            .lock()
            .unwrap()
            .leases
            .get_mut(&f.lease)
            .unwrap()
            .page_generation = Some(3);
        f.relay
            .enqueue(f.admission("write", json!({"kind":"tab.navigate","url":"about:blank"})))
            .unwrap();
        let frame = f.dispatch();
        let result = f.result(
            &frame,
            "succeeded",
            Some(json!({"kind":"tab.navigate","pageGeneration":2,"url":"about:blank"})),
        );
        f.relay
            .accept_result(&f.binding(), "write", result)
            .unwrap();
        assert_eq!(
            f.relay.receipt("write").unwrap().status,
            Status::OutcomeUnknown
        );
        assert_eq!(
            f.relay.state.lock().unwrap().leases[&f.lease].page_generation,
            Some(3)
        );
    }
    #[test]
    fn browser_open_grant_proof_rechecks_live_turn_before_leasing() {
        let f = Fixture::new();
        let live = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let check = Arc::clone(&live);
        let mut admission = f.admission("open", json!({"kind":"tabs.open","url":"about:blank"}));
        admission.authority.current =
            Arc::new(move || check.load(std::sync::atomic::Ordering::SeqCst));
        admission.plan.target.guest_id = None;
        admission.plan.lease_id = None;
        f.relay.enqueue(admission).unwrap();
        f.dispatch();
        assert!(
            super::super::owner::open_proof(&f.relay, &f.binding(), "open", "cutex.s", None)
                .is_ok()
        );
        live.store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            super::super::owner::open_proof(&f.relay, &f.binding(), "open", "cutex.s", None)
                .unwrap_err()
                .code,
            "permission_denied"
        );
        assert!(f.relay.state.lock().unwrap().requests["open"]
            .open_lease
            .is_none());
    }
    #[test]
    fn browser_dispatched_unknown_marks_turn_used_with_known_exact_page() {
        let f = Fixture::new();
        f.relay
            .state
            .lock()
            .unwrap()
            .leases
            .get_mut(&f.lease)
            .unwrap()
            .page_generation = Some(1);
        f.relay
            .enqueue(f.admission(
                "write",
                json!({"kind":"tab.act","action":{"kind":"reload"}}),
            ))
            .unwrap();
        f.dispatch();
        f.relay.cancel("write", "cancelled").unwrap();
        assert_eq!(
            f.relay.receipt("write").unwrap().status,
            Status::OutcomeUnknown
        );
        assert_eq!(
            super::super::shots::take_count_for_test(&f.relay, "cutex.s", "t", 1, "turn1"),
            1
        );
        assert_eq!(
            super::super::shots::take_count_for_test(&f.relay, "cutex.s", "t", 1, "turn1"),
            0
        );
    }
}
