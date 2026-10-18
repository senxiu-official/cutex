//! The only external device authority boundary. Keys stay in request headers.
use super::{
    auth,
    model::*,
    state::{Binding, DesktopIdentity, Relay},
};
use crate::{
    http::server::{write_json_response, SimpleHttpRequest},
    management::server::ManagementRequestContext,
};
use serde_json::{json, Value};
use sha2::Digest;
use std::{
    io::Write,
    net::TcpStream,
    sync::mpsc,
    time::{Duration, Instant},
};

pub fn is_path(path: &str) -> bool {
    path == "/v2/browser" || path.starts_with("/v2/browser/")
}

pub fn handle(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    root: Option<&str>,
    context: ManagementRequestContext,
) -> anyhow::Result<()> {
    crate::http::server::set_keep_alive(crate::http::server::KeepAlive::Off);
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let parts: Vec<_> = path.split('/').collect();
    if path == "/v2/browser/idempotency/clear" {
        return clear_idempotency(stream, request, root);
    }
    let agent_route = path == "/v2/browser/commands"
        || parts.len() >= 5 && parts[1..4] == ["v2", "browser", "commands"];
    let owner_route = matches!(
        path,
        "/v2/browser/agents"
            | "/v2/browser/tool-item"
            | "/v2/browser/leases/grant"
            | "/v2/browser/turn-shots"
    ) || parts.len() == 5 && parts[1..4] == ["v2", "browser", "turn-shots"]
        || parts.len() == 8
            && parts[1..4] == ["v2", "browser", "devices"]
            && parts[5] == "commands"
            && parts[7] == "result";
    if !agent_route && !owner_route {
        return handle_devices(stream, request, root, super::relay());
    }
    let relay = super::relay();
    let reply = (|| -> Result<super::agent_api::Reply> {
        if request.body.len() > MAX_MESSAGE_BYTES {
            return Err(RelayError::new("message_too_large"));
        }
        if agent_route {
            if request.path.contains('?') {
                return Err(RelayError::new("invalid_request"));
            }
            if request.method == "POST" && path == "/v2/browser/commands" {
                return super::agent_api::submit(relay, request, context, stream);
            }
            if parts.len() == 5
                && request.method == "GET"
                && identifier(parts[4])
                && request.body.is_empty()
            {
                return super::agent_api::status(relay, request, context, parts[4], false);
            }
            if parts.len() == 6
                && request.method == "POST"
                && identifier(parts[4])
                && parts[5] == "cancel"
            {
                let _: Empty = parse(&request.body)?;
                return super::agent_api::status(relay, request, context, parts[4], true);
            }
            return Err(RelayError::new("unsupported_operation"));
        }
        let identity = auth::desktop(request, root)?;
        sweep_authenticated(relay);
        let value = if path == "/v2/browser/tool-item"
            && request.method == "GET"
            && request.body.is_empty()
        {
            let query = super::owner::query(&request.path, &["sessionId", "turnId", "itemId"])?;
            super::history::read(context, &query[0], &query[1], &query[2])?
        } else if path == "/v2/browser/turn-shots"
            && request.method == "GET"
            && request.body.is_empty()
        {
            let query = super::owner::query(&request.path, &["sessionId", "turnId"])?;
            super::shot_store::list(&query[0], &query[1])?
        } else {
            if request.path.contains('?') {
                return Err(RelayError::new("invalid_request"));
            }
            if parts.len() == 5
                && parts[1..4] == ["v2", "browser", "turn-shots"]
                && request.method == "GET"
                && request.body.is_empty()
            {
                super::shot_store::read(parts[4])?
            } else if path == "/v2/browser/agents"
                && request.method == "GET"
                && request.body.is_empty()
            {
                super::owner::agents(context)?
            } else if path == "/v2/browser/leases/grant" && request.method == "POST" {
                let body: GrantRequest = parse(&request.body)?;
                let device = body.device_id.clone();
                let binding = binding(request, &identity, &device)?;
                serde_json::to_value(super::owner::grant(relay, &binding, context, body)?)
                    .map_err(|_| RelayError::new("invalid_result"))?
            } else if parts.len() == 8
                && request.method == "POST"
                && identifier(parts[4])
                && identifier(parts[6])
            {
                let binding = binding(request, &identity, parts[4])?;
                let body = super::commands::DeviceResult::parse(&request.body)?;
                let late = relay.accept_result(&binding, parts[6], body)?;
                json!({"accepted":true,"late":late})
            } else {
                return Err(RelayError::new("unsupported_operation"));
            }
        };
        // Disk/history work may have outlasted an in-process or external token
        // revoke. Do not deliver owner-only image/body under a stale identity.
        identity.check()?;
        Ok(super::agent_api::Reply {
            status: 200,
            value: std::sync::Arc::new(value),
            delivered: None,
            retained: None,
        })
    })();
    match reply {
        Ok(reply) => {
            write_json_response(
                stream,
                reply.status,
                "Browser relay response",
                reply.value.as_ref(),
            )?;
            if let Some(key) = reply.delivered {
                relay.delivered(&key);
            }
            Ok(())
        }
        Err(error) => write_json_response(
            stream,
            error.status,
            "Browser relay rejected request",
            &error.value(),
        ),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ClearIdempotencyRequest {
    #[serde(default)]
    cutex_session_id: Option<String>,
}

fn clear_idempotency(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    root: Option<&str>,
) -> anyhow::Result<()> {
    let result = (|| -> Result<Value> {
        if request.method != "POST" || request.path.contains('?') {
            return Err(RelayError::new("unsupported_operation"));
        }
        if !crate::management::v2::server::authenticated_root_request(request, root) {
            return Err(RelayError::new("unauthorized"));
        }
        if !crate::management::listeners::current_request_is_loopback() {
            return Err(RelayError::new("loopback_only"));
        }
        let body: ClearIdempotencyRequest = parse(&request.body)?;
        if body.cutex_session_id.as_deref().is_some_and(|id| {
            id.is_empty() || id.len() > 200 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
        }) {
            return Err(RelayError::new("invalid_request"));
        }
        let counts = crate::management::v2::request_idempotency::request_idempotency_repository()
            .map_err(|_| RelayError::new("browser_unavailable"))?
            .clear_browser_records(body.cutex_session_id.as_deref())
            .map_err(|_| RelayError::new("browser_unavailable"))?;
        super::audit::record(super::audit::Record {
            time: chrono::Utc::now(),
            request_id: format!("clear-{}", uuid::Uuid::new_v4()),
            device_id: "owner".into(),
            generation: String::new(),
            lease_id: None,
            operation_kind: "idempotency.clear".into(),
            status: super::commands::Status::Succeeded,
            error: None,
            duration_ms: 0,
            bytes: 0,
            late_result: 0,
            dropped: 0,
            cleanup: Some(super::audit::Cleanup {
                session_sha256: body.cutex_session_id.as_ref().map(|session| format!("{:x}", sha2::Sha256::digest(session.as_bytes()))),
                outcome_unknown: counts.outcome_unknown,
                abandoned_forwarded: counts.abandoned_forwarded,
            }),
        });
        Ok(json!({
            "deleted": counts.total(),
            "outcomeUnknown": counts.outcome_unknown,
            "abandonedForwarded": counts.abandoned_forwarded,
            "scope": if body.cutex_session_id.is_some() { "session" } else { "all" },
            "warning": "Cleared browser operations no longer have a known result and may execute again if their request keys are retried."
        }))
    })();
    match result {
        Ok(value) => write_json_response(stream, 200, "Browser idempotency cleared", &value),
        Err(error) => write_json_response(stream, error.status, "Browser relay rejected request", &error.value()),
    }
}

fn handle_devices(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    root: Option<&str>,
    relay: &std::sync::Arc<Relay>,
) -> anyhow::Result<()> {
    // A browser connection never leaks its small timeout to a reused HTTP
    // request, nor keeps a reply body indefinitely in a blocked write.
    crate::http::server::set_keep_alive(crate::http::server::KeepAlive::Off);
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let result = (|| -> Result<Option<Value>> {
        if request.body.len() > MAX_MESSAGE_BYTES {
            return Err(RelayError::new("message_too_large"));
        }
        if request.path.contains('?') {
            return Err(RelayError::new("invalid_request"));
        }
        let path = request.path.as_str();
        let identity = auth::desktop(request, root)?;
        let settings = crate::config::browser::current();
        relay.sweep_settings(
            Instant::now(),
            Duration::from_millis(settings.offline_ms),
            Duration::from_secs(settings.lease_idle_secs),
            settings,
        );
        if request.method == "POST" && path == "/v2/browser/devices/register" {
            let registration: Registration = parse(&request.body)?;
            return relay
                .register(
                    identity,
                    registration,
                    settings.heartbeat_ms,
                    settings.offline_ms,
                )
                .map(Some);
        }
        let parts: Vec<_> = path.split('/').collect();
        if parts.len() >= 6 && parts[1..4] == ["v2", "browser", "devices"] && identifier(parts[4]) {
            let binding = binding(request, &identity, parts[4])?;
            match (request.method.as_str(), parts.as_slice()) {
                ("POST", [_, _, _, _, _, "heartbeat"]) => {
                    let body: Heartbeat = parse(&request.body)?;
                    relay.heartbeat_activity(
                        &binding,
                        Duration::from_millis(settings.offline_ms),
                        body.owner_activity,
                    )?;
                    return Ok(Some(json!({"online":true})));
                }
                ("GET", [_, _, _, _, _, "commands", "stream"]) => {
                    if !request.body.is_empty() || request.headers.contains_key("last-event-id") {
                        return Err(RelayError::new("invalid_request"));
                    }
                    let socket = stream
                        .try_clone()
                        .map_err(|_| RelayError::new("browser_unavailable"))?;
                    let subscription = relay.attach(&binding, Some(socket))?;
                    // From here the response is SSE; on any failure close, do
                    // not append a second HTTP/JSON response to this stream.
                    let _ = stream_events(stream, &binding, subscription, relay);
                    return Ok(None);
                }
                _ => return Err(RelayError::new("unsupported_operation")),
            }
        }
        if request.method == "POST"
            && parts.len() == 6
            && parts[1..4] == ["v2", "browser", "leases"]
            && identifier(parts[4])
        {
            let body: DeviceBody = parse(&request.body)?;
            let binding = binding(request, &identity, &body.device_id)?;
            return match parts[5] {
                "activate" => relay.activate(&binding, parts[4]).and_then(|v| {
                    serde_json::to_value(v)
                        .map(Some)
                        .map_err(|_| RelayError::new("invalid_result"))
                }),
                "revoke" => relay
                    .owner_revoke(&binding, parts[4])
                    .map(|_| Some(json!({"revoked":true}))),
                _ => Err(RelayError::new("unsupported_operation")),
            };
        }
        Err(RelayError::new("unsupported_operation"))
    })();
    match result {
        Ok(Some(value)) => write_json_response(stream, 200, "OK", &value),
        Ok(None) => Ok(()),
        Err(error) => write_json_response(
            stream,
            error.status,
            "Browser relay rejected request",
            &error.value(),
        ),
    }
}

pub(crate) fn binding<'a>(
    request: &'a SimpleHttpRequest,
    identity: &'a DesktopIdentity,
    device_id: &'a str,
) -> Result<Binding<'a>> {
    let generation = request
        .headers
        .get("x-cutex-browser-generation")
        .filter(|v| uuid::Uuid::parse_str(v).is_ok())
        .ok_or(RelayError::new("stale_connection"))?;
    let key = request
        .headers
        .get("x-cutex-browser-key")
        .filter(|v| v.len() == 64 && v.bytes().all(|b| matches!(b,b'0'..=b'9'|b'a'..=b'f')))
        .ok_or(RelayError::new("stale_connection"))?;
    if !identifier(device_id) {
        return Err(RelayError::new("invalid_request"));
    }
    Ok(Binding {
        identity,
        device_id,
        generation,
        key,
    })
}
fn stream_events(
    stream: &mut TcpStream,
    binding: &Binding<'_>,
    subscription: super::state::DeviceStream,
    relay: &std::sync::Arc<Relay>,
) -> anyhow::Result<()> {
    stream.write_all(crate::http::server::sse_response_head().as_bytes())?;
    stream.flush()?;
    let mut written = Instant::now();
    loop {
        sweep_authenticated(relay);
        if relay.check_binding_cached(binding).is_err() {
            break;
        }
        match subscription
            .receiver
            .recv_timeout(Duration::from_millis(250))
        {
            Ok(value) => {
                if relay.check_binding(binding).is_err() {
                    break;
                }
                let value = if value.get("kind").is_none() {
                    match relay.prepare_frame(binding, &value) {
                        Ok(Some(frame)) => frame,
                        Ok(None) => continue,
                        Err(_) => break,
                    }
                } else {
                    value
                };
                let bytes = serde_json::to_vec(&value)?;
                anyhow::ensure!(
                    bytes.len() <= MAX_MESSAGE_BYTES,
                    "browser frame exceeds limit"
                );
                stream.write_all(b"event: browser_relay\ndata: ")?;
                stream.write_all(&bytes)?;
                stream.write_all(b"\n\n")?;
                stream.flush()?;
                written = Instant::now();
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if written.elapsed() >= Duration::from_secs(15) {
                    stream.write_all(b": keepalive\n\n")?;
                    stream.flush()?;
                    written = Instant::now();
                }
            }
        }
    }
    Ok(())
}
pub(crate) fn sweep_authenticated(relay: &Relay) {
    let settings = crate::config::browser::current();
    relay.sweep_settings(
        Instant::now(),
        Duration::from_millis(settings.offline_ms),
        Duration::from_secs(settings.lease_idle_secs),
        settings,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::{
        io::{BufRead, BufReader, Read},
        net::TcpListener,
        sync::Arc,
    };
    const TEST_ROOT: &str = "browser-test-root-not-a-real-credential";
    fn unused_context() -> ManagementRequestContext {
        ManagementRequestContext {
            explicit_launch_action: |_, _| panic!("owner auth precedes callback"),
            adopt_saved_native: |_, _| panic!("unused"),
            review_agent_archive: |_, _| panic!("unused"),
            execute_agent_archive: |_, _| panic!("unused"),
            durable_agent_candidates: |_| panic!("unused"),
            import_durable_agent: |_, _| panic!("unused"),
            load_registry: || panic!("unused"),
            load_runtime_status: |_| panic!("unused"),
            forward_native_request: |_, _| panic!("unused"),
            respond_native_server_request: |_, _, _| panic!("unused"),
            handle_user_input: |_| panic!("unused"),
            flush_user_input_queue: |_, _, _| panic!("unused"),
            steer_user_input_queue: |_, _, _| panic!("unused"),
            interrupt_active_turn: |_, _| panic!("unused"),
            load_bootstrap_state: |_, _| panic!("unused"),
            load_latest_plan: |_| panic!("unused"),
            load_latest_token_usage: |_| panic!("unused"),
            revert_session_history: |_, _, _| panic!("unused"),
            delete_session: |_, _| panic!("unused"),
            fork_session: |_, _, _| panic!("unused"),
            side_chat: |_, _, _| panic!("unused"),
            open_offline_history: |_| panic!("unused"),
            mutate_session: |_, _, _| panic!("unused"),
            online_session: |_, _, _| panic!("unused"),
            bind_project_authority: |_| panic!("unused"),
            import_legacy_director_ownership: |_| panic!("unused"),
            reconcile_agent_reservation: |_| panic!("unused"),
            list_management_projects: |_| panic!("unused"),
            read_management_project: |_, _| panic!("unused"),
            update_management_project_presentation: |_, _| panic!("unused"),
            execute_management_operator_action: |_, _| panic!("unused"),
            execute_management_project_mutation: |_, _| panic!("unused"),
            create_human_agent: |_, _| panic!("unused"),
            create_human_session: |_, _| panic!("unused"),
        }
    }
    fn request(method: &str, path: &str, body: Value) -> SimpleHttpRequest {
        SimpleHttpRequest {
            method: method.into(),
            path: path.into(),
            body: if method == "GET" {
                Vec::new()
            } else {
                serde_json::to_vec(&body).unwrap()
            },
            headers: HashMap::from([("authorization".into(), format!("Bearer {TEST_ROOT}"))]),
        }
    }
    fn copied(request: &SimpleHttpRequest) -> SimpleHttpRequest {
        SimpleHttpRequest {
            method: request.method.clone(),
            path: request.path.clone(),
            body: request.body.clone(),
            headers: request.headers.clone(),
        }
    }
    fn sockets() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }
    fn call(relay: &Arc<Relay>, request: &SimpleHttpRequest) -> (u16, Value) {
        let (mut client, mut server) = sockets();
        handle_devices(&mut server, request, Some(TEST_ROOT), relay).unwrap();
        drop(server);
        let mut bytes = Vec::new();
        client.read_to_end(&mut bytes).unwrap();
        let split = bytes.windows(4).position(|v| v == b"\r\n\r\n").unwrap();
        let status = std::str::from_utf8(&bytes[..split])
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        (status, serde_json::from_slice(&bytes[split + 4..]).unwrap())
    }
    fn register(relay: &Arc<Relay>, device: &str) -> Value {
        let (status, value) = call(
            relay,
            &request(
                "POST",
                "/v2/browser/devices/register",
                json!({"deviceId":device,"hostId":"local","providerId":"browser","executor":"inApp"}),
            ),
        );
        assert_eq!(status, 200);
        value
    }
    fn bound(registration: &Value, method: &str, suffix: &str, body: Value) -> SimpleHttpRequest {
        let mut request = request(
            method,
            &format!(
                "/v2/browser/devices/{}/{suffix}",
                registration["deviceId"].as_str().unwrap()
            ),
            body,
        );
        request.headers.insert(
            "x-cutex-browser-generation".into(),
            registration["connectionGeneration"]
                .as_str()
                .unwrap()
                .into(),
        );
        request.headers.insert(
            "x-cutex-browser-key".into(),
            registration["connectionKey"].as_str().unwrap().into(),
        );
        request
    }
    fn stream(
        relay: &Arc<Relay>,
        registration: &Value,
    ) -> (BufReader<TcpStream>, std::thread::JoinHandle<()>) {
        let req = bound(registration, "GET", "commands/stream", Value::Null);
        let (client, mut server) = sockets();
        let relay = Arc::clone(relay);
        let worker = std::thread::spawn(move || {
            handle_devices(&mut server, &req, Some(TEST_ROOT), &relay).unwrap()
        });
        let mut reader = BufReader::new(client);
        let mut head = String::new();
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            head.push_str(&line);
            if line == "\r\n" {
                break;
            }
        }
        assert!(head.starts_with("HTTP/1.1 200"));
        let mut event = String::new();
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            event.push_str(&line);
            if line == "\n" {
                break;
            }
        }
        assert!(event.starts_with("event: browser_relay\ndata: "));
        let value: Value = serde_json::from_str(
            event
                .lines()
                .nth(1)
                .unwrap()
                .strip_prefix("data: ")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(value["kind"], "inventory");
        assert_eq!(value["leases"], json!([]));
        assert!(!event.contains(registration["connectionKey"].as_str().unwrap()));
        (reader, worker)
    }
    #[test]
    fn browser_binding_headers_cannot_be_missing_query_or_malformed() {
        let identity = DesktopIdentity {
            id: "root:test".into(),
            valid: std::sync::Arc::new(|| true),
        };
        let mut request = SimpleHttpRequest {
            method: "POST".into(),
            path: "/v2/browser/devices/d/heartbeat?key=secret".into(),
            body: b"{}".to_vec(),
            headers: HashMap::new(),
        };
        assert!(binding(&request, &identity, "d").is_err());
        request.headers.insert(
            "x-cutex-browser-generation".into(),
            uuid::Uuid::new_v4().to_string(),
        );
        request
            .headers
            .insert("x-cutex-browser-key".into(), "a".repeat(64));
        assert!(binding(&request, &identity, "d").is_ok());
        request
            .headers
            .insert("x-cutex-browser-key".into(), "A".repeat(64));
        assert!(binding(&request, &identity, "d").is_err());
    }
    #[test]
    fn browser_routes_have_an_exact_prefix() {
        assert!(is_path("/v2/browser/devices/register"));
        assert!(!is_path("/v2/browser-other"));
    }
    #[test]
    fn browser_idempotency_clear_requires_root_and_loopback() {
        fn invoke(request: &SimpleHttpRequest) -> (u16, Value) {
            let (mut client, mut server) = sockets();
            handle(&mut server, request, Some(TEST_ROOT), unused_context()).unwrap();
            drop(server);
            let mut bytes = Vec::new(); client.read_to_end(&mut bytes).unwrap();
            let split = bytes.windows(4).position(|v| v == b"\r\n\r\n").unwrap();
            let status = std::str::from_utf8(&bytes[..split]).unwrap().split_whitespace().nth(1).unwrap().parse().unwrap();
            (status, serde_json::from_slice(&bytes[split + 4..]).unwrap())
        }
        let mut unauthenticated = request("POST", "/v2/browser/idempotency/clear", json!({}));
        unauthenticated.headers.clear();
        assert_eq!(invoke(&unauthenticated).0, 401);
        crate::management::listeners::set_current_connection(Some(crate::management::listeners::ConnectionInfo {
            remote: true,
            peer: "127.0.0.1:1".parse().unwrap(),
        }));
        let refused = invoke(&request("POST", "/v2/browser/idempotency/clear", json!({})));
        crate::management::listeners::set_current_connection(None);
        assert_eq!(refused.0, 403);
        assert_eq!(refused.1["error"]["code"], "loopback_only");
    }
    #[test]
    fn browser_http_registration_requires_exact_desktop_auth_and_strict_body() {
        let relay = Arc::new(Relay::new());
        let original = request(
            "POST",
            "/v2/browser/devices/register",
            json!({"deviceId":"d","hostId":"local","providerId":"browser","executor":"inApp"}),
        );
        for auth in [None, Some("Bearer cxa1.fake.session"), Some("Bearer wrong")] {
            let mut req = copied(&original);
            req.headers.remove("authorization");
            if let Some(auth) = auth {
                req.headers.insert("authorization".into(), auth.into());
            }
            assert_eq!(call(&relay, &req).0, 403);
        }
        let mut extra = copied(&original);
        let mut body: Value = serde_json::from_slice(&extra.body).unwrap();
        body["connectionKey"] = json!("spoof");
        extra.body = serde_json::to_vec(&body).unwrap();
        assert_eq!(call(&relay, &extra).1["error"]["code"], "invalid_request");
        body.as_object_mut().unwrap().remove("connectionKey");
        body["providerId"] = json!("computer");
        extra.body = serde_json::to_vec(&body).unwrap();
        assert_eq!(
            call(&relay, &extra).1["error"]["code"],
            "unsupported_provider"
        );
        assert!(relay.state.lock().unwrap().devices.is_empty());
        let registration = register(&relay, "d");
        assert_eq!(registration.as_object().unwrap().len(), 8);
        assert_eq!(registration["connectionKey"].as_str().unwrap().len(), 64);
    }
    #[test]
    fn browser_http_device_binding_rejects_wrong_key_device_generation_and_query() {
        let relay = Arc::new(Relay::new());
        let registration = register(&relay, "d");
        let valid = bound(&registration, "POST", "heartbeat", json!({}));
        for key in ["x-cutex-browser-key", "x-cutex-browser-generation"] {
            let mut req = copied(&valid);
            req.headers.remove(key);
            assert_eq!(call(&relay, &req).1["error"]["code"], "stale_connection");
        }
        let mut req = copied(&valid);
        req.headers
            .insert("x-cutex-browser-key".into(), "0".repeat(64));
        assert_eq!(call(&relay, &req).1["error"]["code"], "stale_connection");
        let mut req = copied(&valid);
        req.path = "/v2/browser/devices/another/heartbeat".into();
        assert_eq!(call(&relay, &req).1["error"]["code"], "stale_connection");
        let mut req = copied(&valid);
        req.path.push_str("?connectionKey=forbidden");
        assert_eq!(call(&relay, &req).1["error"]["code"], "invalid_request");
        let mut req = copied(&valid);
        req.body = br#"{"generation":"spoof"}"#.to_vec();
        assert_eq!(call(&relay, &req).1["error"]["code"], "invalid_request");
        let mut req = valid;
        req.body = vec![b' '; MAX_MESSAGE_BYTES + 1];
        assert_eq!(call(&relay, &req).0, 413);
    }
    #[test]
    fn browser_http_stream_one_per_generation_and_reregister_closes_old_socket() {
        let relay = Arc::new(Relay::new());
        let first = register(&relay, "d");
        let (mut old, worker) = stream(&relay, &first);
        let heartbeat = bound(&first, "POST", "heartbeat", json!({}));
        assert_eq!(call(&relay, &heartbeat), (200, json!({"online":true})));
        let duplicate = bound(&first, "GET", "commands/stream", Value::Null);
        assert_eq!(
            call(&relay, &duplicate).1["error"]["code"],
            "stale_connection"
        );
        let second = register(&relay, "d");
        assert_ne!(
            first["connectionGeneration"],
            second["connectionGeneration"]
        );
        let mut rest = String::new();
        let _ = old.read_to_string(&mut rest);
        worker.join().unwrap();
        assert!(rest.is_empty());
        assert_eq!(
            call(&relay, &heartbeat).1["error"]["code"],
            "stale_connection"
        );
        let mut replay = bound(&second, "GET", "commands/stream", Value::Null);
        replay.headers.insert("last-event-id".into(), "1".into());
        assert_eq!(call(&relay, &replay).1["error"]["code"], "invalid_request");
        let (_reader, second_worker) = stream(&relay, &second);
        relay.disconnect("d", second["connectionGeneration"].as_str().unwrap());
        second_worker.join().unwrap();
    }
    #[test]
    fn browser_http_heartbeat_cannot_make_unconnected_or_offline_device_online() {
        let relay = Arc::new(Relay::new());
        let first = register(&relay, "d");
        let heartbeat = bound(&first, "POST", "heartbeat", json!({}));
        assert_eq!(
            call(&relay, &heartbeat).1["error"]["code"],
            "browser_unavailable"
        );
        assert!(relay.state.lock().unwrap().devices.is_empty());
        let second = register(&relay, "d");
        let (_reader, worker) = stream(&relay, &second);
        relay
            .state
            .lock()
            .unwrap()
            .devices
            .get_mut("d")
            .unwrap()
            .seen = Instant::now() - Duration::from_secs(301);
        let heartbeat = bound(&second, "POST", "heartbeat", json!({}));
        assert_eq!(
            call(&relay, &heartbeat).1["error"]["code"],
            "stale_connection"
        );
        worker.join().unwrap();
    }
    #[test]
    fn browser_http_turn_shots_and_history_reject_valid_cxa1_before_any_owner_callback() {
        let token =
            crate::agent_bus::agent_auth::agent_credential(TEST_ROOT, "runtime-a", "cutex.s");
        for path in [
            "/v2/browser/turn-shots?sessionId=cutex.s&turnId=t",
            "/v2/browser/turn-shots/shot-00000000000000000000000000000001",
            "/v2/browser/tool-item?sessionId=cutex.s&turnId=t&itemId=i",
            "/v2/browser/agents",
        ] {
            let mut req = request("GET", path, Value::Null);
            req.headers
                .insert("authorization".into(), format!("Bearer {token}"));
            req.headers
                .insert("x-cutex-agent-id".into(), "runtime-a".into());
            let (mut client, mut server) = sockets();
            handle(&mut server, &req, Some(TEST_ROOT), unused_context()).unwrap();
            drop(server);
            let mut bytes = Vec::new();
            client.read_to_end(&mut bytes).unwrap();
            assert!(bytes.starts_with(b"HTTP/1.1 403 "));
            let split = bytes.windows(4).position(|b| b == b"\r\n\r\n").unwrap();
            let value: Value = serde_json::from_slice(&bytes[split + 4..]).unwrap();
            assert_eq!(value["error"]["code"], "permission_denied");
            assert!(value.get("items").is_none());
        }
    }
    #[test]
    fn browser_http_paired_token_revoke_synchronously_closes_real_sse_socket() {
        use crate::config::instance::{set_test_selection, Selection};
        let previous = crate::config::instance::selected().unwrap();
        let path =
            std::env::temp_dir().join(format!("browser-pair-revoke-{}", uuid::Uuid::new_v4()));
        set_test_selection(Selection::Custom(path.clone()));
        let store = crate::management::client_tokens::ClientTokenStore::open_default().unwrap();
        let (issued, _) = store
            .issue_device("Browser fixture", "Test desktop", "test")
            .unwrap();
        let mut auth_req = request("POST", "/v2/browser/devices/register", json!({}));
        auth_req
            .headers
            .insert("authorization".into(), format!("Bearer {}", issued.token));
        let verified = crate::management::client_tokens::authenticate_management_request(
            &mut auth_req,
            Some(TEST_ROOT),
        )
        .unwrap();
        crate::management::listeners::set_current_client_token(Some(verified.clone()));
        let relay = super::super::relay();
        let device = format!("test-{}", uuid::Uuid::new_v4().simple());
        let registration = register(relay, &device);
        let req = bound(&registration, "GET", "commands/stream", Value::Null);
        let (client, mut server) = sockets();
        let relay_clone = Arc::clone(relay);
        let thread_path = path.clone();
        let worker = std::thread::spawn(move || {
            set_test_selection(Selection::Custom(thread_path));
            crate::management::listeners::set_current_client_token(Some(verified));
            handle_devices(&mut server, &req, Some(TEST_ROOT), &relay_clone).unwrap();
            crate::management::listeners::set_current_client_token(None);
            set_test_selection(Selection::Default);
        });
        let mut reader = BufReader::new(client);
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            if line == "\r\n" {
                break;
            }
        }
        let mut event = String::new();
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            event.push_str(&line);
            if line == "\n" {
                break;
            }
        }
        assert!(event.contains("\"kind\":\"inventory\""));
        assert!(store.revoke(&issued.token_id).unwrap());
        // The synchronous revoke hook, not a later sweep/timer, has removed
        // authority when revoke() returns. The held TCP stream is shut down.
        assert!(!relay.state.lock().unwrap().devices.contains_key(&device));
        let mut tail = Vec::new();
        reader.read_to_end(&mut tail).unwrap();
        assert!(tail.is_empty());
        worker.join().unwrap();
        assert_eq!(
            call(relay, &bound(&registration, "POST", "heartbeat", json!({}))).1["error"]["code"],
            "permission_denied"
        );
        crate::management::listeners::set_current_client_token(None);
        set_test_selection(previous);
        drop(store);
        std::fs::remove_dir_all(path).unwrap();
    }
}
