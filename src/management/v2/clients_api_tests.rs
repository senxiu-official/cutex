use super::*;
use crate::management::users::Grant;

fn t0() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-19T10:00:00Z").unwrap().with_timezone(&Utc)
}

fn presence(instance: &str, allow: bool) -> Presence {
    Presence {
        install_id: format!("install-{instance}"),
        instance_id: instance.into(),
        app_name: "cutex-desktop".into(),
        app_version: "0.9.0".into(),
        device_name: format!("pc-{instance}"),
        platform: "linux".into(),
        allow_remote_close: allow,
    }
}

fn root() -> Caller {
    Caller { token_id: None, principal: Principal::Owner, local: true, scope: DeviceScope::Full }
}

fn token(id: &str, principal: Principal, scope: DeviceScope) -> Caller {
    Caller { token_id: Some(id.into()), principal, local: false, scope }
}

fn user(id: &str) -> Principal {
    Principal::User { id: id.into(), name: id.into(), grant: Grant::default() }
}

fn ids(items: &[Value]) -> Vec<&str> {
    items.iter().map(|item| item["instanceId"].as_str().unwrap()).collect()
}

#[test]
fn presence_registers_heartbeats_and_expires_after_three_missed_heartbeats() {
    let mut registry = Registry::default();
    let caller = token("mct-a", user("u-a"), DeviceScope::Read);
    let first = registry.heartbeat(presence("i-1", true), &caller, t0()).unwrap().clone();
    assert_eq!((first.connected_since, first.last_seen, first.state), (t0(), t0(), ClientState::Online));
    assert_eq!(first.user_id.as_deref(), Some("u-a"));
    // A heartbeat updates the reported fields and lastSeen, keeps connectedSince.
    let mut changed = presence("i-1", false);
    changed.app_version = "0.9.1".into();
    let later = t0() + Duration::seconds(60);
    let second = registry.heartbeat(changed, &caller, later).unwrap();
    assert_eq!((second.connected_since, second.last_seen), (t0(), later));
    assert_eq!((second.presence.app_version.as_str(), second.presence.allow_remote_close), ("0.9.1", false));
    // Exactly 3 × 60 s without a heartbeat: still there; one second more: gone.
    assert_eq!(registry.visible(&caller, later + Duration::seconds(EXPIRE_SECONDS)).len(), 1);
    assert!(registry.visible(&caller, later + Duration::seconds(EXPIRE_SECONDS + 1)).is_empty());
    assert!(registry.is_empty());
    // Re-registering after expiry is a new connection.
    let again = t0() + Duration::seconds(1000);
    assert_eq!(registry.heartbeat(presence("i-1", true), &caller, again).unwrap().connected_since, again);
}

#[test]
fn an_instance_id_belongs_to_its_registering_credential() {
    let mut registry = Registry::default();
    registry.heartbeat(presence("i-1", true), &token("mct-a", Principal::Owner, DeviceScope::Full), t0()).unwrap();
    let error = registry.heartbeat(presence("i-1", true), &root(), t0()).unwrap_err();
    assert_eq!((error.status, error.code), (409, "client_instance_conflict"));
    let error = registry
        .report_closing("i-1", &token("mct-b", Principal::Owner, DeviceScope::Full), ClientState::Closing, t0())
        .unwrap_err();
    assert_eq!((error.status, error.code, &error.details["reason"]), (403, "client_close_forbidden", &json!("notRegistrant")));
    // One credential keeps at most MAX_PER_CREDENTIAL instances (oldest dropped).
    let caller = token("mct-c", Principal::Pending, DeviceScope::Full);
    for n in 0..=MAX_PER_CREDENTIAL {
        registry.heartbeat(presence(&format!("c-{n}"), true), &caller, t0() + Duration::seconds(n as i64)).unwrap();
    }
    let mine = registry.visible(&caller, t0() + Duration::seconds(20));
    assert_eq!(mine.len(), MAX_PER_CREDENTIAL);
    assert!(!ids(&mine).contains(&"c-0"));
}

#[test]
fn list_visibility_per_principal() {
    let mut registry = Registry::default();
    let owner_full = root();
    let owner_chat = token("mct-oc", Principal::Owner, DeviceScope::Chat);
    let alice = token("mct-a", user("u-a"), DeviceScope::Full);
    let alice_phone = token("mct-a2", user("u-a"), DeviceScope::Read);
    let bob = token("mct-b", user("u-b"), DeviceScope::Full);
    let pending = token("mct-p", Principal::Pending, DeviceScope::Full);
    let other_pending = token("mct-q", Principal::Pending, DeviceScope::Full);
    for (instance, caller) in [("o-1", &owner_full), ("a-1", &alice), ("b-1", &bob), ("p-1", &pending), ("q-1", &other_pending)] {
        registry.heartbeat(presence(instance, true), caller, t0()).unwrap();
    }
    let now = t0() + Duration::seconds(5);
    let all = registry.visible(&owner_full, now);
    assert_eq!(all.len(), 5);
    let own = all.iter().find(|item| item["instanceId"] == "o-1").unwrap();
    assert_eq!((&own["current"], &own["userId"], &own["tokenId"], &own["local"]), (&json!(true), &json!("owner"), &Value::Null, &json!(true)));
    let pending_item = all.iter().find(|item| item["instanceId"] == "p-1").unwrap();
    assert_eq!((&pending_item["current"], &pending_item["userId"], &pending_item["tokenId"]), (&json!(false), &Value::Null, &json!("mct-p")));
    // A narrowed owner device: the owner's own instances only.
    assert_eq!(ids(&registry.visible(&owner_chat, now)), vec!["o-1"]);
    // Users: their own user's instances, every device of theirs.
    assert_eq!(ids(&registry.visible(&alice, now)), vec!["a-1"]);
    let from_phone = registry.visible(&alice_phone, now);
    assert_eq!((ids(&from_phone), &from_phone[0]["current"]), (vec!["a-1"], &json!(false)));
    assert_eq!(ids(&registry.visible(&bob, now)), vec!["b-1"]);
    // Pending: only what its own token registered.
    assert_eq!(ids(&registry.visible(&pending, now)), vec!["p-1"]);
    // Every field of the contract.
    let keys: Vec<&String> = own.as_object().unwrap().keys().collect();
    for key in ["instanceId", "installId", "appName", "appVersion", "deviceName", "platform", "userId", "tokenId", "local", "connectedSince", "lastSeen", "allowRemoteClose", "state", "current"] {
        assert!(keys.iter().any(|k| *k == key), "{key}");
    }
    assert_eq!(own["connectedSince"], "2026-10-19T10:00:00.000Z");
}

#[test]
fn close_permission_matrix() {
    let owner_full = root();
    let owner_token = token("mct-of", Principal::Owner, DeviceScope::Full);
    let owner_chat = token("mct-oc", Principal::Owner, DeviceScope::Chat);
    let alice = token("mct-a", user("u-a"), DeviceScope::Full);
    let alice_chat = token("mct-ac", user("u-a"), DeviceScope::Chat);
    let alice_read = token("mct-ar", user("u-a"), DeviceScope::Read);
    let bob = token("mct-b", user("u-b"), DeviceScope::Full);
    let pending = token("mct-p", Principal::Pending, DeviceScope::Full);
    let fresh = || {
        let mut registry = Registry::default();
        registry.heartbeat(presence("o-1", true), &root(), t0()).unwrap();
        registry.heartbeat(presence("a-1", true), &token("mct-a", user("u-a"), DeviceScope::Full), t0()).unwrap();
        registry.heartbeat(presence("p-1", true), &token("mct-p", Principal::Pending, DeviceScope::Full), t0()).unwrap();
        registry.heartbeat(presence("n-1", false), &token("mct-a", user("u-a"), DeviceScope::Full), t0()).unwrap();
        registry
    };
    let close = |caller: &Caller, instance: &str| {
        fresh().request_close(instance, caller, "dev", DEFAULT_GRACE_SECONDS, t0()).map(|o| o.state).map_err(|e| {
            (e.status, e.code, e.details["reason"].as_str().map(str::to_string))
        })
    };
    let forbidden = |reason: &str| Err((403, "client_close_forbidden", Some(reason.to_string())));
    // The owner with full scope closes anything.
    for instance in ["o-1", "a-1", "p-1"] {
        assert_eq!(close(&owner_full, instance), Ok(ClientState::CloseRequested), "{instance}");
        assert_eq!(close(&owner_token, instance), Ok(ClientState::CloseRequested), "{instance}");
    }
    // Narrowed scopes never close (owner or user, own instance or not).
    assert_eq!(close(&owner_chat, "o-1"), forbidden("scope"));
    assert_eq!(close(&alice_chat, "a-1"), forbidden("scope"));
    assert_eq!(close(&alice_read, "a-1"), forbidden("scope"));
    // Same user: yes; another user / the owner's: no.
    assert_eq!(close(&alice, "a-1"), Ok(ClientState::CloseRequested));
    assert_eq!(close(&bob, "a-1"), forbidden("otherUser"));
    assert_eq!(close(&alice, "o-1"), forbidden("otherUser"));
    assert_eq!(close(&alice, "p-1"), forbidden("otherUser"));
    // Pending devices never close, not even their own instance.
    assert_eq!(close(&pending, "p-1"), forbidden("pending"));
    // Unknown instance; the instance's own switch.
    assert_eq!(close(&owner_full, "nope"), Err((404, "client_not_found", None)));
    assert_eq!(close(&owner_full, "n-1"), Err((409, "remote_close_disabled", None)));
    assert_eq!(close(&bob, "n-1"), forbidden("otherUser"), "permission before the switch");
}

#[test]
fn close_request_then_outcome() {
    let mut registry = Registry::default();
    let desktop = token("mct-a", user("u-a"), DeviceScope::Full);
    registry.heartbeat(presence("a-1", true), &desktop, t0()).unwrap();
    let requester = Caller { local: true, ..root() };
    let outcome = registry.request_close("a-1", &requester, "host-pc", 20, t0()).unwrap();
    assert_eq!(
        outcome.event.unwrap(),
        json!({"instanceId": "a-1", "requestedBy": {"deviceName": "host-pc", "userId": "owner", "local": true}, "graceSeconds": 20, "at": "2026-10-19T10:00:00.000Z"})
    );
    // The heartbeat now carries the open request.
    let instance = registry.heartbeat(presence("a-1", true), &desktop, t0()).unwrap();
    assert_eq!(instance.state, ClientState::CloseRequested);
    assert_eq!(instance.close_request_view()["graceSeconds"], 20);
    // Cancelled by the person at the desktop; a new request opens again.
    registry.report_closing("a-1", &desktop, ClientState::Cancelled, t0()).unwrap();
    let instance = registry.get("a-1").unwrap();
    assert_eq!((instance.state, instance.close_request_view()), (ClientState::Cancelled, Value::Null));
    registry.request_close("a-1", &requester, "host-pc", 15, t0()).unwrap();
    registry.report_closing("a-1", &desktop, ClientState::Closing, t0()).unwrap();
    // Already closing: accepted, no new event.
    let again = registry.request_close("a-1", &requester, "host-pc", 15, t0()).unwrap();
    assert_eq!((again.state, again.event), (ClientState::Closing, None));
    assert_eq!(registry.device_name_of(&desktop).as_deref(), Some("pc-a-1"));
    assert_eq!(registry.user_of_instance("a-1").as_deref(), Some("u-a"));
}

#[test]
fn paths_and_presence_validation() {
    assert_eq!(parse_path("/v2/clients"), Some(Route::List));
    assert_eq!(parse_path("/v2/clients/presence"), Some(Route::Presence));
    assert_eq!(parse_path("/v2/clients/abc-1/close"), Some(Route::Close("abc-1".into())));
    assert_eq!(parse_path("/v2/clients/abc-1/closing"), Some(Route::Closing("abc-1".into())));
    for bad in ["/v2/clients/", "/v2/clients/abc", "/v2/clients/presence/close", "/v2/clients/a b/close", "/v2/clients/a/close/x"] {
        assert_eq!(parse_path(bad), None, "{bad}");
    }
    let object = |value: Value| value.as_object().unwrap().clone();
    let ok = parse_presence(&object(json!({"installId": "in-1", "instanceId": "i-1", "appName": "cutex-desktop"}))).unwrap();
    assert!(ok.allow_remote_close, "default on");
    for bad in [
        json!({"instanceId": "i-1", "appName": "x"}),
        json!({"installId": "in-1", "instanceId": "i 1", "appName": "x"}),
        json!({"installId": "in-1", "instanceId": "i-1", "appName": ""}),
        json!({"installId": "in-1", "instanceId": "i-1", "appName": "x", "allowRemoteClose": "yes"}),
        json!({"installId": "in-1", "instanceId": "i-1", "appName": "x", "deviceName": "a".repeat(200)}),
    ] {
        assert_eq!(parse_presence(&object(bad.clone())).unwrap_err().code, "invalid_request", "{bad}");
    }
}
