//! Additive persisted policy fields remain readable; PATCH inputs stay strict.
use super::{
    policy::{Caps, Export, Policy, Review, Visibility},
    store::Store,
    tests,
};
use serde_json::{json, Value};
use std::path::PathBuf;

struct TestRoot(PathBuf);
impl TestRoot {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("peer-policy-review2-{}", uuid::Uuid::new_v4()));
        crate::management::client_tokens::ensure_private_dir(&path).unwrap();
        Self(path)
    }
    fn store(&self) -> Store {
        Store::at(self.0.join("runtime/peers"))
    }
}
impl Drop for TestRoot {
    fn drop(&mut self) {
        let removed = std::fs::remove_dir_all(&self.0);
        if !std::thread::panicking() {
            removed.expect("remove private policy test directory");
        }
    }
}

fn configured_policy() -> Policy {
    Policy::default().patched(&json!({
        "inbound": {
            "exports": [{"agent":"agent-one"},{"project":"project-one"},{"all":false}],
            "accept":false,
            "review":"ifTargetWider",
            "assumedCaps":{"sandbox":"workspace-write","networkAccess":true,"unrestrictedServices":false},
            "maxOpenAsks":42,"asksPerHour":73
        },
        "outbound":{"visibility":"all"}
    })).unwrap()
}

fn assert_rejected(policy: &Policy, patch: Value) {
    let before = policy.clone();
    let error = policy.patched(&patch).unwrap_err();
    assert_eq!(error.code, "invalid_request", "{patch}");
    assert_eq!(error.status, 400, "{patch}");
    assert_eq!(policy, &before);
}

#[test]
fn full_peer_store_accepts_future_fields_at_every_policy_level() {
    let root = TestRoot::new();
    let store = root.store();
    let mut peer = tests::peer();
    peer.policy = configured_policy();
    store
        .with(|state| {
            state.settings.enabled = true;
            state.peers.insert(peer.peer_id.clone(), peer.clone());
            Ok(())
        })
        .unwrap();
    let path = store.dir.join("peers.json");
    let mut persisted: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    persisted["futureStoreField"] = json!({"version":2});
    persisted["settings"]["futureSetting"] = json!(true);
    let saved_peer = &mut persisted["peers"][&peer.peer_id];
    saved_peer["futurePeerField"] = json!(["future"]);
    let policy = &mut saved_peer["policy"];
    policy["futurePolicyField"] = json!(false);
    policy["inbound"]["futureInboundField"] = json!({"new":true});
    policy["outbound"]["futureOutboundField"] = json!(7);
    policy["inbound"]["assumedCaps"]["futureCapability"] = json!("future");
    for export in policy["inbound"]["exports"].as_array_mut().unwrap() {
        export["futureExportField"] = json!({"enabled":true});
    }
    let bytes = serde_json::to_vec_pretty(&persisted).unwrap();
    std::fs::write(&path, &bytes).unwrap();

    let loaded = store.snapshot().unwrap();
    assert!(loaded.settings.enabled);
    assert_eq!(loaded.peers[&peer.peer_id], peer);
    assert_eq!(store.get(&peer.peer_id).unwrap(), peer);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        bytes,
        "read/no-op must not rewrite future fields"
    );
    store
        .with(|state| {
            state.peers.get_mut(&peer.peer_id).unwrap().enabled = false;
            Ok(())
        })
        .unwrap();
    let loaded = store.snapshot().unwrap();
    assert!(!loaded.peers[&peer.peer_id].enabled);
    assert_eq!(loaded.peers[&peer.peer_id].policy, peer.policy);
}

#[test]
fn policy_patch_rejects_unknown_keys_at_every_object_level() {
    let policy = configured_policy();
    for patch in [
        json!({"futurePolicyField":true}),
        json!({"inbound":{"accept":true,"futureInboundField":true}}),
        json!({"outbound":{"visibility":"none","futureOutboundField":true}}),
        json!({"inbound":{"exports":[{"agent":"known","futureExportField":true}]}}),
        json!({"inbound":{"exports":[{"project":"known","futureExportField":true}]}}),
        json!({"inbound":{"exports":[{"all":true,"futureExportField":true}]}}),
        json!({"inbound":{"exports":[{"futureExportField":true}]}}),
        json!({"inbound":{"assumedCaps":{"sandbox":null,"networkAccess":false,"unrestrictedServices":false,"futureCapability":true}}}),
        json!({"inbound":{"assumedCaps":{"sandbox":null,"networkAccess":false,"unrestricted_services":false}}}),
        json!(null),
        json!([]),
        json!({"inbound":null}),
        json!({"outbound":[]}),
    ] {
        assert_rejected(&policy, patch);
    }
}

#[test]
fn policy_patch_rejects_ambiguous_selectors_and_accept_aliases() {
    let policy = configured_policy();
    for patch in [
        json!({"inbound":{"asks":true,"accept":false}}),
        json!({"inbound":{"asks":true,"accept":true}}),
        json!({"inbound":{"asks":false,"accept":false}}),
        json!({"inbound":{"exports":[{}]}}),
        json!({"inbound":{"exports":[{"agent":"a","project":"p"}]}}),
        json!({"inbound":{"exports":[{"agent":"a","all":true}]}}),
        json!({"inbound":{"exports":[{"project":"p","all":false}]}}),
        json!({"inbound":{"exports":[{"agent":"a","project":"p","all":true}]}}),
    ] {
        assert_rejected(&policy, patch);
    }
    let accept = policy.patched(&json!({"inbound":{"accept":true}})).unwrap();
    let asks = policy.patched(&json!({"inbound":{"asks":true}})).unwrap();
    assert_eq!(accept, asks);
    assert!(accept.inbound.asks);
    assert_eq!(accept.inbound.exports, policy.inbound.exports);
}

#[test]
fn strict_patch_keeps_the_existing_value_domains_and_replacement_semantics() {
    let policy = configured_policy();
    for (open, hourly) in [(1, 1), (10000, 100000)] {
        let patched = policy
            .patched(&json!({"inbound":{"maxOpenAsks":open,"asksPerHour":hourly}}))
            .unwrap();
        assert_eq!(patched.inbound.max_open_asks, open);
        assert_eq!(patched.inbound.asks_per_hour, hourly);
    }
    for patch in [
        json!({"inbound":{"maxOpenAsks":0}}),
        json!({"inbound":{"maxOpenAsks":10001}}),
        json!({"inbound":{"asksPerHour":0}}),
        json!({"inbound":{"asksPerHour":100001}}),
        json!({"inbound":{"maxOpenAsks":-1}}),
        json!({"inbound":{"asksPerHour":1.5}}),
        json!({"inbound":{"accept":"yes"}}),
        json!({"inbound":{"review":"futureReview"}}),
        json!({"outbound":{"visibility":"futureVisibility"}}),
        json!({"inbound":{"exports":null}}),
        json!({"inbound":{"exports":[null]}}),
        json!({"inbound":{"exports":[{"agent":42}]}}),
        json!({"inbound":{"exports":[{"project":false}]}}),
        json!({"inbound":{"exports":[{"all":"true"}]}}),
        json!({"inbound":{"assumedCaps":null}}),
        json!({"inbound":{"assumedCaps":{"sandbox":1,"networkAccess":false,"unrestrictedServices":false}}}),
        json!({"inbound":{"assumedCaps":{"networkAccess":false}}}),
    ] {
        assert_rejected(&policy, patch);
    }
    for sandbox in [Value::Null, json!("future-sandbox")] {
        let patched = policy.patched(&json!({"inbound":{"assumedCaps":{"sandbox":sandbox,"networkAccess":false,"unrestrictedServices":true}}})).unwrap();
        assert!(!patched.inbound.assumed_caps.network_access);
        assert!(patched.inbound.assumed_caps.unrestricted_services);
        assert!(patched.needs_review(&Caps::default()));
    }
    let replaced = policy
        .patched(&json!({"inbound":{"exports":[{"all":false},{"agent":""},{"project":""}]}}))
        .unwrap();
    assert_eq!(
        replaced.inbound.exports,
        vec![
            Export::All { all: false },
            Export::Agent {
                agent: String::new()
            },
            Export::Project {
                project: String::new()
            }
        ]
    );
    assert!(policy
        .patched(&json!({"inbound":{"exports":[]}}))
        .unwrap()
        .inbound
        .exports
        .is_empty());
    assert_eq!(policy.patched(&json!({})).unwrap(), policy);
    assert_eq!(policy.inbound.review, Review::IfTargetWider);
    assert_eq!(policy.outbound.visibility, Visibility::All);
}

#[test]
fn rejected_nested_policy_patch_leaves_the_complete_store_unchanged() {
    let root = TestRoot::new();
    let store = root.store();
    let mut peer = tests::peer();
    peer.policy = configured_policy();
    store
        .with(|state| {
            state.peers.insert(peer.peer_id.clone(), peer.clone());
            Ok(())
        })
        .unwrap();
    let path = store.dir.join("peers.json");
    let before = std::fs::read(&path).unwrap();
    let error = store.with(|state| {
        let current = state.peers.get_mut(&peer.peer_id).unwrap();
        current.enabled = false;
        current.policy = current.policy.patched(&json!({"inbound":{"accept":true,"assumedCaps":{"sandbox":null,"networkAccess":true,"unrestrictedServices":false,"typo":true}}}))?;
        Ok(())
    }).unwrap_err();
    assert_eq!(error.code, "invalid_request");
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(store.snapshot().unwrap().peers[&peer.peer_id], peer);
}
