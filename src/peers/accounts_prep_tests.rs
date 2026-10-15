//! P1 preparation only: P0 binds owner and keeps its existing export policy.
use super::{audit, pair, policy, routes, store, tests::peer, token};
use serde_json::{json, Value};

#[test]
fn old_peer_records_bind_owner_and_explicit_other_users_are_rejected() {
    let mut legacy = serde_json::to_value(peer()).unwrap();
    legacy.as_object_mut().unwrap().remove("userId");
    let migrated: store::Peer = serde_json::from_value(legacy.clone()).unwrap();
    assert_eq!(migrated.user_id, "owner");
    assert_eq!(migrated.view()["userId"], "owner");
    for invalid in [json!("u-other"), json!(""), Value::Null, json!(1)] {
        legacy["userId"] = invalid;
        assert!(serde_json::from_value::<store::Peer>(legacy.clone()).is_err());
    }
    let dir = std::env::temp_dir().join(format!("peer-owner-only-{}", uuid::Uuid::new_v4()));
    let store = store::Store::at(dir.clone());
    let mut other = peer();
    other.user_id = "u-other".into();
    assert_eq!(
        store.insert(other, &token::generate()).unwrap_err().code,
        "peer_user_unsupported"
    );
    assert!(
        !dir.exists(),
        "reject an unsupported binding before creating state or secrets"
    );
}

#[test]
fn pairing_user_selectors_are_validated_before_network_codes_or_storage() {
    let dir = std::env::temp_dir().join(format!("peer-user-selector-{}", uuid::Uuid::new_v4()));
    let store = store::Store::at(dir.clone());
    for key in ["userId", "targetUserId"] {
        assert_eq!(
            pair::requested_owner_user(&json!({}), key).unwrap(),
            "owner"
        );
        assert_eq!(
            pair::requested_owner_user(&json!({key:"owner"}), key).unwrap(),
            "owner"
        );
        for invalid in [json!("u-other"), json!(""), Value::Null, json!(42)] {
            let expected = if invalid.is_string() {
                "peer_user_unsupported"
            } else {
                "invalid_request"
            };
            let body = json!({key:invalid});
            // Bodies intentionally lack codes and URLs. The user selector must
            // be rejected first, without consuming an invite or attempting I/O.
            assert_eq!(pair::add(&store, &body).unwrap_err().code, expected);
            assert_eq!(pair::receive(&store, &body).unwrap_err().code, expected);
            assert_eq!(pair::invite(&body).unwrap_err().code, expected);
        }
    }
    assert!(!dir.exists());
}

#[test]
fn visibility_and_delivery_share_one_local_policy_boundary() {
    let mut peer = peer();
    let mut agent = crate::asks::directory::tests::agent("cutex.local", "worker");
    agent.project = Some("project-local".into());
    peer.policy.outbound.visibility = policy::Visibility::All;
    assert!(
        !policy::visible(&peer, &agent),
        "outbound visibility does not grant inbound access"
    );
    assert!(!policy::can_deliver(&peer, &agent));
    peer.policy.inbound.exports = vec![policy::Export::Project {
        project: "project-local".into(),
    }];
    assert!(policy::visible(&peer, &agent));
    assert!(policy::can_deliver(&peer, &agent));
    peer.policy.inbound.asks = false;
    assert!(
        policy::visible(&peer, &agent),
        "read visibility is independent of send permission"
    );
    assert!(!policy::can_deliver(&peer, &agent));
    peer.policy.inbound.asks = true;
    for mutation in ["nonowner", "disabled", "pending", "removed"] {
        let mut denied = peer.clone();
        match mutation {
            "nonowner" => denied.user_id = "u-not-implemented".into(),
            "disabled" => denied.enabled = false,
            "pending" => denied.pending_until = Some(super::now()),
            "removed" => denied.status = "removedByPeer".into(),
            _ => unreachable!(),
        }
        assert!(!policy::visible(&denied, &agent), "{mutation}");
        assert!(!policy::can_deliver(&denied, &agent), "{mutation}");
    }
    agent.archived = true;
    assert!(!policy::visible(&peer, &agent));
    agent.archived = false;
    agent.agent_enabled = false;
    assert!(!policy::can_deliver(&peer, &agent));
}

struct PrivateDirectory {
    root: std::path::PathBuf,
    previous: crate::config::instance::Selection,
}

impl PrivateDirectory {
    fn new() -> Self {
        let root = crate::config::paths::home_dir()
            .unwrap()
            .join(format!("peer-local-directory-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(crate::config::paths::PRIVATE_HOME_MARKER), b"").unwrap();
        let previous = crate::config::instance::selected().unwrap();
        crate::config::instance::set_test_selection(crate::config::instance::Selection::Custom(
            root.join(".cutex"),
        ));
        store::Store::open()
            .unwrap()
            .settings(&json!({"enabled":true}))
            .unwrap();
        Self { root, previous }
    }
}

impl Drop for PrivateDirectory {
    fn drop(&mut self) {
        crate::config::instance::set_test_selection(self.previous.clone());
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn exported_directory_never_reexports_remote_cache_or_mirror_targets() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let instance = PrivateDirectory::new();
    let local = "cutex.00000000-0000-0000-0000-000000000001";
    let remote = "cutex.00000000-0000-0000-0000-000000000002";
    let mut sessions = crate::session::model::CutexSessionStore::default();
    let mut record = crate::session::model::CutexSessionRecord::new(
        local.into(),
        None,
        "fixture-host".into(),
        instance.root.to_string_lossy().into(),
        None,
    )
    .unwrap();
    record.agent_enabled = true;
    record.formal_agent_name = Some("local-worker".into());
    sessions.sessions.insert(local.into(), record);
    crate::session::store::save_cutex_session_store(&sessions).unwrap();

    let store = store::Store::open().unwrap();
    let mut peer = peer();
    peer.policy.inbound.exports = vec![policy::Export::All { all: true }];
    crate::config::atomic::write_private_pretty_json_atomic(
        &store.dir.join(format!("directory-{}.json", peer.peer_id)),
        &json!({"fetchedAt":super::now(),"agents":[{"cutexSessionId":remote,"name":"foreign-worker","short":"foreign","status":"idle","runtime":"codex"}]}),
        "foreign directory fixture",
    ).unwrap();
    let (mut state, mut mirror, _) = super::tests::remote_record();
    mirror.target_cutex_session_id = remote.into();
    state.asks.insert(mirror.ask_id.clone(), mirror);
    crate::asks::store::AskStore::open_default()
        .unwrap()
        .with_state(|current| *current = state)
        .unwrap();

    let exported = routes::exported(&peer).unwrap();
    let listed = exported["agents"].as_array().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["cutexSessionId"], local);
    assert!(!listed.iter().any(|agent| agent["cutexSessionId"] == remote));
    super::asks::validate_inbound(&peer, local).unwrap();
    assert_eq!(
        super::asks::validate_inbound(&peer, remote)
            .unwrap_err()
            .code,
        "recipient_outside_boundary"
    );
}

#[test]
fn audit_records_the_bound_user_and_unbound_pairing() {
    let dir = std::env::temp_dir().join(format!("peer-acting-user-{}", uuid::Uuid::new_v4()));
    let store = store::Store::at(dir.clone());
    let peer = peer();
    audit::record(
        &store,
        "inbound",
        Some(&peer),
        "/peer/v1/hello",
        &Ok(json!({})),
        0,
        None,
    );
    audit::record(
        &store,
        "inbound",
        None,
        "/peer/v1/pair",
        &Ok(json!({})),
        0,
        None,
    );
    let text = std::fs::read_to_string(dir.join("peer-audit.jsonl")).unwrap();
    let entries: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(entries[0]["actingUser"], "owner");
    assert!(entries[1]["actingUser"].is_null());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_peer_bound_to_a_local_user_sees_exactly_that_users_grant() {
    use crate::management::users::{Grant, Scope};
    let mut peer = peer();
    peer.user_id = "u-0123456789ab".into();
    // Export policy no longer applies to a non-owner binding.
    peer.policy.inbound.exports = vec![policy::Export::All { all: true }];
    let granted = crate::asks::directory::tests::agent("cutex.granted", "granted");
    let other = crate::asks::directory::tests::agent("cutex.other", "other");
    let read_only = |user: &str| {
        (user == "u-0123456789ab").then(|| Scope::for_tests(Grant::default(), &["cutex.granted"]))
    };
    let with_send = |user: &str| {
        (user == "u-0123456789ab")
            .then(|| Scope::for_tests(Grant { send: true, ..Grant::default() }, &["cutex.granted"]))
    };
    assert!(policy::visible_with(&peer, &granted, read_only));
    assert!(!policy::visible_with(&peer, &other, read_only), "outside the grant, even with exports all");
    assert!(!policy::can_deliver_with(&peer, &granted, read_only), "read-only grant cannot ask");
    assert!(policy::can_deliver_with(&peer, &granted, with_send));
    assert!(!policy::can_deliver_with(&peer, &other, with_send));
    peer.policy.inbound.asks = false;
    assert!(!policy::can_deliver_with(&peer, &granted, with_send), "the peer switch still applies");
    // A deleted user (no scope) sees nothing.
    assert!(!policy::visible_with(&peer, &granted, |_| None));
}
