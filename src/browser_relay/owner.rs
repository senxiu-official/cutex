//! Owner-only relay operations. Caller authentication remains in http.rs.
use super::{
    model::*,
    state::{self, Binding, Relay},
};
use crate::management::server::ManagementRequestContext;
use serde_json::{json, Value};
use std::sync::Arc;

pub struct Agent {
    pub session: String,
    pub name: String,
    pub thread: String,
    pub generation: u64,
    pub runtime: String,
    /// The session runs with full access (完全访问), see [`full_access`].
    pub full_access: bool,
}
pub fn agent(context: ManagementRequestContext, session: &str) -> Result<Agent> {
    if !identifier(session) {
        return Err(RelayError::new("invalid_request"));
    }
    let store = crate::session::store::load_cutex_session_store_shared()
        .map_err(|_| RelayError::new("browser_unavailable"))?;
    let profiles = crate::profiles::settings_api::load_store()
        .map_err(|_| RelayError::new("browser_unavailable"))?;
    let record = store
        .sessions
        .get(session)
        .filter(|r| {
            r.cutex_session_id == session
                && (r.registration_class
                    == crate::agent_bus::model::AgentRegistrationClass::Persistent
                    || r.is_owned_session())
        })
        .ok_or(RelayError::new("browser_disabled"))?;
    agent_record(context, record, &profiles)
}
fn agent_record(
    context: ManagementRequestContext,
    record: &crate::session::model::CutexSessionRecord,
    profiles: &crate::profiles::model::AccountsStore,
) -> Result<Agent> {
    let session = record.cutex_session_id.as_str();
    // Persistent agents, and the owner's quick chats (owned sessions: same
    // browser rules, no Agent identity), whose profile opted in.
    if !(record.is_owned_session()
        || record.registration_class == crate::agent_bus::model::AgentRegistrationClass::Persistent)
        || !crate::agent_bus::tool_policy::browser_tools_for_record_in(record, profiles)
    {
        return Err(RelayError::new("browser_disabled"));
    }
    let status = (context.load_runtime_status)(session)
        .map_err(|_| RelayError::new("browser_unavailable"))?
        .filter(|s| {
            s.connected
                && s.cutex_session_id == session
                && s.runtime_generation == record.runtime_generation
                && record.app_server_thread_id() == Some(s.thread_id.as_str())
        })
        .ok_or(RelayError::new("browser_unavailable"))?;
    let runtime = record
        .current_runtime_agent_id
        .clone()
        .filter(|id| identifier(id))
        .ok_or(RelayError::new("browser_unavailable"))?;
    let raw = record
        .display_name_override
        .as_deref()
        .or(record.formal_agent_name.as_deref())
        .or(record.display_name_hint.as_deref())
        // A quick chat without a hint yet: its native thread title.
        .or(record.thread_name.as_deref())
        .unwrap_or(session);
    let mut name: String = raw.chars().filter(|c| !c.is_control()).take(200).collect();
    if name.trim().is_empty() {
        name = session.into();
    }
    let full_access = full_access(record, status.thread_settings.as_ref());
    Ok(Agent {
        session: session.into(),
        name,
        thread: status.thread_id,
        generation: status.runtime_generation,
        runtime,
        full_access,
    })
}

/// Whether a session runs with full access (the desktop's 完全访问): native
/// sandbox `danger-full-access` together with approval policy `never`.
///
/// Source: the live native thread settings of the connected runtime (the
/// resume snapshot, then every `thread/settings/updated`, i.e. what the
/// thread runs with now, including a mode switched after launch). A field
/// those settings do not carry falls back to the session record's stored
/// permissions (`sandbox_mode` / `approval_policy` / `permission_defaults`:
/// what the runtime was launched with). Anything unknown is not full access.
pub fn full_access(
    record: &crate::session::model::CutexSessionRecord,
    thread_settings: Option<&Value>,
) -> bool {
    let live_sandbox = thread_settings.and_then(|settings| {
        let policy = settings.get("sandboxPolicy").or_else(|| settings.get("sandbox"))?;
        policy.get("type").and_then(Value::as_str).or_else(|| policy.as_str()).map(str::to_string)
    });
    let live_approval = thread_settings
        .and_then(|settings| settings.get("approvalPolicy"))
        .map(|policy| policy.as_str().map(str::to_string));
    let (stored_sandbox, stored_approval) =
        crate::runtime::args::effective_runtime_permission_defaults(record);
    let sandbox = live_sandbox.or(stored_sandbox);
    // A non-string approval policy (granular rules) is never `never`.
    let approval = match live_approval {
        Some(live) => live,
        None => stored_approval,
    };
    let full_sandbox = sandbox
        .as_deref()
        .is_some_and(|sandbox| matches!(sandbox, "dangerFullAccess" | "danger-full-access"));
    full_sandbox && approval.as_deref() == Some("never")
}
pub fn agents(context: ManagementRequestContext) -> Result<Value> {
    let store = crate::session::store::load_cutex_session_store_shared()
        .map_err(|_| RelayError::new("browser_unavailable"))?;
    let mut sessions: Vec<_> = store.sessions.keys().cloned().collect();
    let profiles = crate::profiles::settings_api::load_store()
        .map_err(|_| RelayError::new("browser_unavailable"))?;
    sessions.sort();
    let mut items = Vec::new();
    for session in sessions {
        if let Some(record) = store.sessions.get(&session) {
            if let Ok(agent) = agent_record(context, record, &profiles) {
                items.push(json!({"sessionId":agent.session,"name":agent.name}));
            }
        }
    }
    let value = json!({"items":items});
    if serde_json::to_vec(&value)
        .map_err(|_| RelayError::new("invalid_result"))?
        .len()
        > MAX_MESSAGE_BYTES
    {
        return Err(RelayError::new("message_too_large"));
    }
    Ok(value)
}
pub fn grant(
    relay: &Arc<Relay>,
    binding: &Binding<'_>,
    context: ManagementRequestContext,
    request: GrantRequest,
) -> Result<LeaseView> {
    // Validate device proof before looking up another session or consuming an
    // approval, and check it again in grant_checked after any store I/O.
    relay.check_binding(binding)?;
    if request.device_id != binding.device_id
        || !identifier(&request.guest_id)
        || !identifier(&request.cutex_session_id)
        || !display_text(&request.title, 200)
        || safe_url(&request.url).is_empty()
    {
        return Err(RelayError::new("invalid_request"));
    }
    let agent = agent(context, &request.cutex_session_id)?;
    match &request.source {
        GrantSource::Owner {} => {}
        GrantSource::Claim { approval_id } => {
            if !identifier(approval_id) {
                return Err(RelayError::new("invalid_request"));
            }
            let host = {
                let state = relay
                    .state
                    .lock()
                    .map_err(|_| RelayError::new("browser_unavailable"))?;
                state::validate_binding(&state, binding)?
                    .registration
                    .host_id
                    .clone()
            };
            let store = crate::approvals::ApprovalStore::open_default()
                .map_err(|_| RelayError::new("browser_unavailable"))?;
            let record = store
                .consume_browser_claim(
                    approval_id,
                    &agent.session,
                    &host,
                    binding.device_id,
                    binding.generation,
                    &agent.thread,
                    agent.generation,
                    chrono::Utc::now(),
                )
                .map_err(|_| RelayError::new("browser_unavailable"))?
                .ok_or(RelayError::new("approval_denied"))?;
            crate::management::v2::approvals::publish_changed(&record, "consumed");
        }
        GrantSource::AgentOpen { request_id } => {
            open_proof(relay, binding, request_id, &agent.session, None)?;
        }
        // The opener lease is checked atomically in grant_checked.
        GrantSource::Popup { opener_lease_id } => {
            if !identifier(opener_lease_id) {
                return Err(RelayError::new("invalid_request"));
            }
        }
    }
    let open_id = match &request.source {
        GrantSource::AgentOpen { request_id } => Some(request_id.clone()),
        _ => None,
    };
    let lease = relay.grant_checked(binding, request, agent.name)?;
    if let Some(id) = open_id {
        if let Err(error) = open_proof(relay, binding, &id, &agent.session, Some(&lease.lease_id)) {
            let _ = relay.owner_revoke(binding, &lease.lease_id);
            return Err(error);
        }
    }
    // A runtime change during approval/store I/O cannot receive a new lease.
    if !self::agent(context, &agent.session).is_ok_and(|now| {
        now.thread == agent.thread
            && now.generation == agent.generation
            && now.runtime == agent.runtime
    }) {
        let _ = relay.owner_revoke(binding, &lease.lease_id);
        return Err(RelayError::new("permission_denied"));
    }
    Ok(lease)
}
pub(crate) fn open_proof(
    relay: &Arc<Relay>,
    binding: &Binding<'_>,
    request_id: &str,
    session: &str,
    lease: Option<&str>,
) -> Result<()> {
    if !identifier(request_id) {
        return Err(RelayError::new("invalid_request"));
    }
    let current = {
        let state = relay
            .state
            .lock()
            .map_err(|_| RelayError::new("browser_unavailable"))?;
        state::validate_binding(&state, binding)?;
        let request = state
            .requests
            .values()
            .find(|r| {
                r.request_id == request_id
                    && r.target.device_id == binding.device_id
                    && r.generation == binding.generation
                    && r.authority.agent.session == session
                    && r.operation_kind == "tabs.open"
                    && r.status.sent()
                    && r.open_lease.is_none()
                    && std::time::Instant::now() < r.deadline
            })
            .ok_or(RelayError::new("permission_denied"))?;
        Arc::clone(&request.authority.current)
    };
    if !current() {
        return Err(RelayError::new("permission_denied"));
    }
    binding.identity.check()?;
    let mut state = relay
        .state
        .lock()
        .map_err(|_| RelayError::new("browser_unavailable"))?;
    state::validate_binding(&state, binding)?;
    let request = state
        .requests
        .values_mut()
        .find(|r| {
            r.request_id == request_id
                && r.target.device_id == binding.device_id
                && r.generation == binding.generation
                && r.authority.agent.session == session
                && r.operation_kind == "tabs.open"
                && r.status.sent()
                && r.open_lease.is_none()
                && std::time::Instant::now() < r.deadline
        })
        .ok_or(RelayError::new("permission_denied"))?;
    if let Some(lease) = lease {
        request.open_lease = Some(lease.into());
    }
    Ok(())
}

/// Query arguments are not a generic URL/router surface. Reject duplicates,
/// unknown names, malformed escapes and missing values before any lookup.
pub fn query(path: &str, names: &[&str]) -> Result<Vec<String>> {
    let (_, raw) = path
        .split_once('?')
        .ok_or(RelayError::new("invalid_request"))?;
    if raw.len() > 2048 || raw.contains('#') {
        return Err(RelayError::new("invalid_request"));
    }
    let bytes = raw.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'%'
            && (index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit())
        {
            return Err(RelayError::new("invalid_request"));
        }
    }
    let mut values = std::collections::HashMap::new();
    for (name, value) in url::form_urlencoded::parse(bytes) {
        if !names.contains(&name.as_ref())
            || !identifier(&value)
            || values
                .insert(name.into_owned(), value.into_owned())
                .is_some()
        {
            return Err(RelayError::new("invalid_request"));
        }
    }
    names
        .iter()
        .map(|name| {
            values
                .remove(*name)
                .ok_or(RelayError::new("invalid_request"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn full_access_prefers_live_thread_settings_and_falls_back_to_the_record() {
        let path = if cfg!(windows) { "C:\\work" } else { "/work" };
        let mut record = crate::session::model::CutexSessionRecord::new(
            "cutex.00000000-0000-0000-0000-000000000001".into(),
            None,
            "host".into(),
            path.into(),
            None,
        )
        .unwrap();
        // Nothing known: not full access.
        assert!(!full_access(&record, None));
        record.sandbox_mode = Some("danger-full-access".into());
        record.approval_policy = Some("never".into());
        assert!(full_access(&record, None));
        // The live thread switched to workspace-write / on-request: not full.
        let auto = json!({"approvalPolicy":"on-request","sandboxPolicy":{"type":"workspaceWrite"}});
        assert!(!full_access(&record, Some(&auto)));
        // Launched restricted, switched to full access while running.
        record.sandbox_mode = Some("workspace-write".into());
        record.approval_policy = Some("on-request".into());
        let full = json!({"approvalPolicy":"never","sandboxPolicy":{"type":"dangerFullAccess"}});
        assert!(full_access(&record, Some(&full)));
        // Resume snapshot form (sandbox as a string) and partial settings.
        assert!(full_access(&record, Some(&json!({"approvalPolicy":"never","sandboxPolicy":"danger-full-access"}))));
        assert!(!full_access(&record, Some(&json!({"sandboxPolicy":{"type":"dangerFullAccess"}}))));
        record.approval_policy = Some("never".into());
        assert!(full_access(&record, Some(&json!({"sandboxPolicy":{"type":"dangerFullAccess"}}))));
        // Full sandbox with approvals, or granular approval rules: not full.
        assert!(!full_access(&record, Some(&json!({"approvalPolicy":"on-request","sandboxPolicy":{"type":"dangerFullAccess"}}))));
        assert!(!full_access(&record, Some(&json!({"approvalPolicy":{"granular":{}},"sandboxPolicy":{"type":"dangerFullAccess"}}))));
        // The permission alias alone (no explicit fields).
        record.sandbox_mode = None;
        record.approval_policy = None;
        record.permission_defaults = Some("full-access".into());
        assert!(full_access(&record, None));
    }
    #[test]
    fn browser_claim_owner_device_and_host_alias_are_bound_from_current_registration() {
        use super::super::state::DesktopIdentity;
        let relay = Arc::new(Relay::new());
        let identity = DesktopIdentity {
            id: "root:test".into(),
            valid: Arc::new(|| true),
        };
        let registration = |id: &str, host: &str| Registration {
            device_id: id.into(),
            host_id: host.into(),
            provider_id: "browser".into(),
            executor: "inApp".into(),
        };
        let local = relay
            .register(
                identity.clone(),
                registration("desktop-local", "local"),
                10000,
                30000,
            )
            .unwrap();
        let remote = relay
            .register(
                identity.clone(),
                registration("desktop-other", "ssh-alias"),
                10000,
                30000,
            )
            .unwrap();
        let proof = |value: &Value| {
            (
                value["connectionGeneration"].as_str().unwrap().to_owned(),
                value["connectionKey"].as_str().unwrap().to_owned(),
            )
        };
        let (local_generation, local_key) = proof(&local);
        let (generation, key) = proof(&remote);
        let local_binding = Binding {
            identity: &identity,
            device_id: "desktop-local",
            generation: &local_generation,
            key: &local_key,
        };
        let binding = Binding {
            identity: &identity,
            device_id: "desktop-other",
            generation: &generation,
            key: &key,
        };
        let _a = relay.attach(&local_binding, None).unwrap();
        let _b = relay.attach(&binding, None).unwrap();
        let root =
            std::env::temp_dir().join(format!("browser-claim-alias-{}", uuid::Uuid::new_v4()));
        let store = crate::approvals::ApprovalStore::at(root.join("approvals.json"));
        let now = chrono::Utc::now();
        let pending=store.request_browser_claim(crate::approvals::ApprovalRequest{kind:crate::approvals::ApprovalKind::BrowserClaim,reason:"owner-selection".into(),requester_cutex_session_id:"cutex.s".into(),requester_name:None,project_id:None,summary:"select".into(),details:json!({"hostId":"local","deviceId":"desktop-local","connectionGeneration":local_generation,"threadId":"t","runtimeGeneration":1}),request_sha256:"digest".into()},now).unwrap();
        let selected = store
            .consume_browser_claim(
                &pending.approval_id,
                "cutex.s",
                "ssh-alias",
                binding.device_id,
                binding.generation,
                "t",
                1,
                now,
            )
            .unwrap()
            .unwrap();
        let lease = relay
            .grant_checked(
                &binding,
                GrantRequest {
                    device_id: binding.device_id.into(),
                    guest_id: "g".into(),
                    cutex_session_id: "cutex.s".into(),
                    title: "page".into(),
                    url: "about:blank".into(),
                    source: GrantSource::Claim {
                        approval_id: pending.approval_id,
                    },
                },
                "Agent".into(),
            )
            .unwrap();
        assert_eq!(lease.target.host_id, "ssh-alias");
        assert_eq!(lease.target.device_id, binding.device_id);
        assert_eq!(selected.details["hostId"], lease.target.host_id);
        assert_eq!(selected.details["connectionGeneration"], binding.generation);
        assert_eq!(
            relay
                .activate(&local_binding, &lease.lease_id)
                .unwrap_err()
                .code,
            "permission_denied"
        );
        relay
            .register(
                identity.clone(),
                registration("desktop-other", "ssh-alias"),
                10000,
                30000,
            )
            .unwrap();
        assert!(relay.activate(&binding, &lease.lease_id).is_err());
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn browser_owner_query_is_exact_bounded_and_non_path_bearing() {
        assert_eq!(
            query(
                "/v2/browser/turn-shots?sessionId=cutex.s&turnId=turn1",
                &["sessionId", "turnId"]
            )
            .unwrap(),
            vec!["cutex.s", "turn1"]
        );
        for raw in [
            "sessionId=cutex.s&turnId=t&token=x",
            "sessionId=cutex.s&turnId=t&turnId=t",
            "sessionId=cutex.s",
            "sessionId=..%2Fsecret&turnId=t",
            "sessionId=x%GG&turnId=t",
            "sessionId=x&turnId=t#secret",
        ] {
            assert!(query(
                &format!("/v2/browser/turn-shots?{raw}"),
                &["sessionId", "turnId"]
            )
            .is_err());
        }
    }
}
