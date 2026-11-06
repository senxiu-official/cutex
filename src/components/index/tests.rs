use super::*;
use crate::upgrade::minisign::test_key::TestKey;
use crate::upgrade::stage::fixtures::{installed_layout, temp};

fn document(sequence: u64) -> Value {
    json!({"schema":"cutex/components-index/v1","channel":"public-stable","sequence":sequence,"publishedAt":"2026-10-29T00:00:00Z",
        "components":[{"name":"fake-tool","recommended":"v1","releases":[{"version":"v1","payload":"bundled-v1",
        "compatible":{"allCapabilities":[]},"assetTag":"component-fake-tool-v1","platforms":{platform_key():{
        "name":"fake-tool-v1.tar.gz","sha256":"a".repeat(64),"size":10,"signatureName":"fake-tool-v1.tar.gz.minisig"}},
        "yanked":false,"yankReason":null,"externalRuntime":null,"externalRuntimeSha256":null}]}]})
}
fn pair(doc: &Value, key: &TestKey) -> (Vec<u8>, String) {
    let bytes = serde_json::to_vec(doc).unwrap();
    let signature = key.sign(&bytes, true, "file:index.json");
    (bytes, signature)
}
fn accept_doc(layout: &Layout, key: &TestKey, doc: &Value) -> anyhow::Result<Value> {
    let (bytes, sig) = pair(doc, key);
    accept(
        layout,
        &[key.public()],
        doc["channel"].as_str().unwrap(),
        &bytes,
        &sig,
    )
}
fn policy(layout: &Layout, source: &std::path::Path) {
    Policy {
        policy: "componentIndex".into(),
        source: Some(source.display().to_string()),
        ..Default::default()
    }
    .save(layout)
    .unwrap();
}
fn code(error: anyhow::Error) -> String {
    error
        .downcast_ref::<crate::upgrade::stage::UpgradeError>()
        .map(|e| e.code.to_string())
        .unwrap_or_else(|| error.to_string())
}

#[test]
fn signed_acceptance_replay_equivocation_and_wrong_pair_leave_last_good() {
    let layout = installed_layout("0.1.1", 50);
    let key = TestKey::new(81);
    let doc = document(4);
    accept_doc(&layout, &key, &doc).unwrap();
    accept_doc(&layout, &key, &doc).unwrap();
    let before = std::fs::read(state_path(&layout)).unwrap();
    assert_eq!(
        code(accept_doc(&layout, &key, &document(3)).unwrap_err()),
        "component_index_replay"
    );
    let mut changed = doc.clone();
    changed["publishedAt"] = json!("2026-10-29T00:00:01Z");
    assert_eq!(
        code(accept_doc(&layout, &key, &changed).unwrap_err()),
        "component_index_equivocation"
    );
    let (mut bytes, sig) = pair(&document(5), &key);
    bytes.push(b' ');
    assert!(accept(&layout, &[key.public()], "public-stable", &bytes, &sig).is_err());
    let (bytes, sig) = pair(&document(5), &key);
    assert!(accept(
        &layout,
        &[TestKey::new(82).public()],
        "public-stable",
        &bytes,
        &sig
    )
    .is_err());
    assert!(accept(&layout, &[key.public()], "wrong-channel", &bytes, &sig).is_err());
    assert_eq!(std::fs::read(state_path(&layout)).unwrap(), before);
    assert_eq!(
        cached(
            &layout,
            &[key.public()],
            &read_state(&layout).unwrap(),
            "public-stable"
        )
        .unwrap()
        .unwrap()
        .sequence,
        4
    );
}

#[test]
fn offline_refresh_keeps_cached_offers_and_does_not_change_selected_defaults() {
    let layout = installed_layout("0.1.1", 50);
    let key = TestKey::new(83);
    let source = temp("component-index");
    policy(&layout, &source);
    let selected = BTreeMap::from([(
        "fake-tool".into(),
        super::super::Entry {
            selected: "r2".into(),
            origin: "signed".into(),
            ..Default::default()
        },
    )]);
    super::super::save_state(&layout, &selected).unwrap();
    accept_doc(&layout, &key, &document(1)).unwrap();
    assert!(refresh(&layout, &[key.public()]).is_err());
    let resolved = resolve(&layout, &[key.public()], "fake-tool", Some("v1")).unwrap();
    assert_eq!(resolved.offer.version, "v1");
    let before = std::fs::read(state_path(&layout)).unwrap();
    let row = view(&layout, &[key.public()], "fake-tool", Some("r2"));
    assert_eq!(row["recommended"], "v1");
    assert_eq!(row["stale"], true);
    assert!(row["fetchedAt"].is_string());
    assert_eq!(std::fs::read(state_path(&layout)).unwrap(), before);
    assert_eq!(super::super::load_state(&layout), selected);
}

#[test]
fn corrupt_cache_and_corrupt_state_never_reset_sequence() {
    let layout = installed_layout("0.1.1", 50);
    let key = TestKey::new(84);
    policy(&layout, &temp("component-index"));
    accept_doc(&layout, &key, &document(7)).unwrap();
    let state = read_state(&layout).unwrap();
    let path = blob(&layout, &state.channels["public-stable"].digest);
    std::fs::write(&path, b"corrupt").unwrap();
    assert!(resolve(&layout, &[key.public()], "fake-tool", Some("v1")).is_err());
    assert_eq!(
        view(&layout, &[key.public()], "fake-tool", None)["stale"],
        true
    );
    assert_eq!(
        code(accept_doc(&layout, &key, &document(6)).unwrap_err()),
        "component_index_replay"
    );
    std::fs::write(state_path(&layout), b"corrupt retained").unwrap();
    assert!(accept_doc(&layout, &key, &document(8)).is_err());
    assert!(refresh(&layout, &[key.public()]).is_err());
    assert_eq!(
        std::fs::read(state_path(&layout)).unwrap(),
        b"corrupt retained"
    );
}

#[test]
fn cross_channel_immutable_identity_and_sticky_withdrawal() {
    let layout = installed_layout("0.1.1", 50);
    let key = TestKey::new(85);
    policy(&layout, &temp("component-index"));
    accept_doc(&layout, &key, &document(1)).unwrap();
    let mut other = document(1);
    other["channel"] = json!("private-r269");
    other["components"][0]["releases"][0]["platforms"][platform_key()]["sha256"] =
        json!("b".repeat(64));
    assert_eq!(
        code(accept_doc(&layout, &key, &other).unwrap_err()),
        "component_identity_conflict"
    );
    let mut yank = document(2);
    yank["components"][0]["recommended"] = Value::Null;
    yank["components"][0]["releases"][0]["yanked"] = json!(true);
    accept_doc(&layout, &key, &yank).unwrap();
    assert_eq!(
        code(resolve(&layout, &[key.public()], "fake-tool", Some("v1")).unwrap_err()),
        "component_yanked"
    );
    assert!(
        view(&layout, &[key.public()], "fake-tool", Some("v1"))["selectedYanked"]
            .as_bool()
            .unwrap()
    );
    other["components"][0]["releases"][0]["platforms"][platform_key()]["sha256"] =
        json!("a".repeat(64));
    accept_doc(&layout, &key, &other).unwrap();
    Policy {
        policy: "componentIndex".into(),
        channel: "private-r269".into(),
        ..Default::default()
    }
    .save(&layout)
    .unwrap();
    assert_eq!(
        code(resolve(&layout, &[key.public()], "fake-tool", Some("v1")).unwrap_err()),
        "component_yanked"
    );
    assert_eq!(
        view(&layout, &[key.public()], "fake-tool", Some("v1"))["recommended"],
        Value::Null
    );
}

#[test]
fn concurrent_acceptance_keeps_highest_watermark_and_matching_blob() {
    let layout = installed_layout("0.1.1", 50);
    let key = TestKey::new(86);
    accept_doc(&layout, &key, &document(1)).unwrap();
    let mut joins = Vec::new();
    for sequence in [2, 3] {
        let layout = layout.clone();
        let key = TestKey::new(86);
        joins.push(std::thread::spawn(move || {
            accept_doc(&layout, &key, &document(sequence))
        }));
    }
    let results: Vec<_> = joins.into_iter().map(|j| j.join().unwrap()).collect();
    assert!(results[1].is_ok());
    let state = read_state(&layout).unwrap();
    assert_eq!(state.channels["public-stable"].sequence, 3);
    assert_eq!(
        cached(&layout, &[key.public()], &state, "public-stable")
            .unwrap()
            .unwrap()
            .sequence,
        3
    );
}

#[test]
fn signed_snapshot_from_local_mirror_and_unpaired_refresh() {
    let layout = installed_layout("0.1.1", 50);
    let key = TestKey::new(87);
    let source = temp("component-index");
    policy(&layout, &source);
    let tag = source.join("channel-components");
    std::fs::create_dir_all(&tag).unwrap();
    let (bytes, sig) = pair(&document(1), &key);
    std::fs::write(tag.join("index.json"), bytes).unwrap();
    std::fs::write(tag.join("index.json.minisig"), sig).unwrap();
    assert_eq!(refresh(&layout, &[key.public()]).unwrap()["sequence"], 1);
    std::fs::write(tag.join("index.json"), pair(&document(2), &key).0).unwrap();
    assert!(refresh(&layout, &[key.public()]).is_err());
    assert_eq!(
        read_state(&layout).unwrap().channels["public-stable"].sequence,
        1
    );
}

#[test]
fn resign_same_document_keeps_prior_signature_blob_until_atomic_record_switch() {
    let layout = installed_layout("0.1.1", 50);
    let old = TestKey::new(88);
    let new = TestKey::new(89);
    let doc = document(1);
    accept_doc(&layout, &old, &doc).unwrap();
    let before = read_state(&layout).unwrap();
    let old_signature_path =
        signature_blob(&layout, &before.channels["public-stable"].signature_digest);
    let old_signature = std::fs::read(&old_signature_path).unwrap();
    let (bytes, sig) = pair(&doc, &new);
    let new_path = signature_blob(&layout, &sha256_hex(sig.as_bytes()));
    crate::config::atomic::write_bytes_atomic(&new_path, sig.as_bytes()).unwrap();
    assert!(
        cached(
            &layout,
            &[old.public()],
            &read_state(&layout).unwrap(),
            "public-stable"
        )
        .unwrap()
        .is_some(),
        "interruption before state commit leaves prior accepted pair readable"
    );
    accept(
        &layout,
        &[old.public(), new.public()],
        "public-stable",
        &bytes,
        &sig,
    )
    .unwrap();
    let state = read_state(&layout).unwrap();
    assert_eq!(
        state.channels["public-stable"].key_id,
        new.public().key_id_hex()
    );
    assert!(cached(&layout, &[new.public()], &state, "public-stable")
        .unwrap()
        .is_some());
    assert_eq!(std::fs::read(old_signature_path).unwrap(), old_signature);
}
