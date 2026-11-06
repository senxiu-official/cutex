use super::*;

fn who() -> Identity {
    Identity {
        doc_key: "owner".into(),
        user_id: "owner".into(),
        fingerprint: None,
        display_name: "Owner".into(),
        owner_person_confirmed: false,
    }
}

struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn temp() -> (Temp, LayoutStore) {
    let root = std::env::temp_dir().join(format!("layout-{}", uuid::Uuid::new_v4()));
    (Temp(root.clone()), LayoutStore::at(root.join("users")))
}

#[test]
fn layout_roundtrip_conflict_delete_and_unknown_sections() {
    let (_temp, store) = temp();
    let who = who();
    assert!(store.get(&who).unwrap().sections.is_empty());
    assert!(!store.dir().exists(), "read must not create storage");
    let data = json!({"members":[{"host":"ci-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","id":"offline"},
        {"hostLabel":"missing","localId":"old-host::project"}],"future":{"preserve":true}});
    assert_eq!(
        store
            .put(&who, "sidebar", 0, data.clone())
            .unwrap()
            .revision,
        1
    );
    assert_eq!(
        store
            .put(&who, "sidebar", 1, data.clone())
            .unwrap()
            .revision,
        1
    );
    let e = store.put(&who, "sidebar", 0, data.clone()).unwrap_err();
    assert_eq!(
        (e.status, e.code, e.details),
        (409, "layout_revision_conflict", json!({"revision":1}))
    );
    store
        .put(&who, "future-section", 0, json!({"x":true}))
        .unwrap();
    assert_eq!(store.get(&who).unwrap().sections["sidebar"].data, data);
    assert_eq!(
        store.put(&who, "sidebar", 1, Value::Null).unwrap().revision,
        2
    );
    assert_eq!(
        store.put(&who, "sidebar", 2, Value::Null).unwrap().revision,
        2
    );
    assert_eq!(
        store
            .put(&who, "sidebar", 0, data.clone())
            .unwrap_err()
            .status,
        409
    );
    assert_eq!(store.put(&who, "sidebar", 2, data).unwrap().revision, 3);
    assert_eq!(
        store.put(&who, "empty", 0, Value::Null).unwrap().revision,
        0
    );
    assert!(!store.get(&who).unwrap().sections.contains_key("empty"));
    let audit = std::fs::read_to_string(store.users_dir.join("audit.jsonl")).unwrap();
    assert_eq!(
        audit.lines().count(),
        4,
        "no-op/conflict writes must not audit"
    );
    assert!(audit.contains("\"viaSwitch\":false"));
}

#[test]
fn layout_size_limits_and_names() {
    let (_temp, store) = temp();
    let who = who();
    for name in ["", "../bad", "A", "a/b", "a_b", "pointer", &"a".repeat(33)] {
        assert_eq!(
            store.put(&who, name, 0, json!({})).unwrap_err().code,
            "invalid_layout"
        );
    }
    for data in [json!([]), json!(true), json!("str"), json!(7)] {
        assert_eq!(store.put(&who, "sidebar", 0, data).unwrap_err().status, 400);
    }
    // {"x":"..."} has eight bytes of overhead in compact JSON.
    let exact = json!({"x":"x".repeat(MAX_SECTION_BYTES-8)});
    assert_eq!(serde_json::to_vec(&exact).unwrap().len(), MAX_SECTION_BYTES);
    store.put(&who, "sidebar", 0, exact.clone()).unwrap();
    assert_eq!(
        store
            .put(
                &who,
                "oversize",
                0,
                json!({"x":"x".repeat(MAX_SECTION_BYTES-7)})
            )
            .unwrap_err()
            .code,
        "layout_too_large"
    );
    for i in 0..6 {
        store
            .put(&who, &format!("future-{i}"), 0, exact.clone())
            .unwrap();
    }
    let e = store.put(&who, "exceeds-document", 0, exact).unwrap_err();
    assert_eq!(
        (e.status, e.details["limit"].as_u64()),
        (413, Some(MAX_DOCUMENT_BYTES as u64))
    );
    assert!(!store
        .get(&who)
        .unwrap()
        .sections
        .contains_key("exceeds-document"));
}

#[test]
fn layout_pointer_is_personal_and_preserves_sections() {
    let (_temp, store) = temp();
    let who = who();
    store.put(&who, "sidebar", 0, json!({"x":1})).unwrap();
    let pointer = store
        .set_pointer(&who, "ci-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "host-a")
        .unwrap();
    assert_eq!(
        store
            .set_pointer(&who, &pointer.memory_host, &pointer.label)
            .unwrap(),
        pointer
    );
    assert_eq!(store.get(&who).unwrap().sections["sidebar"].revision, 1);
    assert_eq!(
        store.set_pointer(&who, "local", "EVA").unwrap_err().status,
        400
    );
    assert_eq!(
        store
            .set_pointer(&who, &pointer.memory_host, "a\nb")
            .unwrap_err()
            .status,
        400
    );
    let mut other = who.clone();
    other.doc_key = "owner~0123456789abcdef".into();
    other.fingerprint = Some("0123456789abcdef".into());
    assert!(store.get(&other).unwrap().pointer.is_none());
    store
        .put(&other, "sidebar", 0, json!({"different":true}))
        .unwrap();
    assert!(store.dir().join("owner--0123456789abcdef.json").exists());
    assert_eq!(
        store.get(&who).unwrap().sections["sidebar"].data,
        json!({"x":1})
    );
}

#[test]
fn layout_person_derivation() {
    use base64::Engine;
    let a = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([1u8; 32]);
    let b = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([2u8; 32]);
    assert_eq!(derive_doc_key("owner", true, Some(&b), Some(&a)), "owner");
    assert_eq!(derive_doc_key("owner", false, Some(&a), Some(&a)), "owner");
    assert_eq!(derive_doc_key("owner", false, None, Some(&a)), "owner");
    assert_eq!(derive_doc_key("owner", false, Some(&b), None), "owner");
    assert_eq!(
        derive_doc_key("owner", false, Some(&b), Some(&a)),
        format!("owner~{}", super::super::persons::fingerprint(&b))
    );
    assert_eq!(
        derive_doc_key("u-0123456789ab", false, Some(&b), Some(&a)),
        "u-0123456789ab"
    );
    assert!(!valid_doc_key("owner~../bad"));
    assert!(!valid_doc_key("owner~ABCDEF0123456789"));
}

#[test]
fn layout_fail_closed_and_preserve_future_document_fields() {
    let (_temp, store) = temp();
    let who = who();
    store.put(&who, "sidebar", 0, json!({"x":1})).unwrap();
    let path = store.path(&who.doc_key);
    let mut record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    record["futureDocumentField"] = json!({"keep":true});
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    store.put(&who, "sidebar", 1, json!({"x":2})).unwrap();
    let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["futureDocumentField"], json!({"keep":true}));
    std::fs::write(&path, b"broken json").unwrap();
    assert_eq!(store.get(&who).unwrap_err().code, "layout_unavailable");
    assert_eq!(
        store.put(&who, "sidebar", 0, json!({})).unwrap_err().status,
        503
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"broken json");
    let mut bad = who.clone();
    bad.doc_key = "../escape".into();
    assert_eq!(store.get(&bad).unwrap_err().status, 400);
    assert_eq!(
        store.put(&bad, "sidebar", 0, json!({})).unwrap_err().status,
        400
    );
}

#[test]
fn layout_sixteen_threads_sections_and_compare_exchange() {
    let (_temp, store) = temp();
    let mut threads = vec![];
    for i in 0..16 {
        let store = store.clone();
        threads.push(std::thread::spawn(move || {
            store
                .put(&who(), &format!("section-{i}"), 0, json!({"i":i}))
                .unwrap();
            loop {
                let current = store.get(&who()).unwrap().sections.get("counter").cloned();
                let revision = current.as_ref().map_or(0, |s| s.revision);
                let n = current
                    .as_ref()
                    .map_or(0, |s| s.data["n"].as_u64().unwrap());
                match store.put(&who(), "counter", revision, json!({"n":n+1})) {
                    Ok(_) => break,
                    Err(e) if e.status == 409 => continue,
                    Err(e) => panic!("{e:?}"),
                }
            }
        }));
    }
    for t in threads {
        t.join().unwrap();
    }
    let record = store.get(&who()).unwrap();
    assert_eq!(record.sections.len(), 17);
    assert_eq!(record.sections["counter"].revision, 16);
    assert_eq!(record.sections["counter"].data["n"], 16);
}

#[test]
#[cfg(debug_assertions)]
fn layout_refuses_operation_lane() {
    let (_temp, store) = temp();
    let lane = super::super::v2::session_coordination::OperationLane::new();
    let held = lane.reserve().unwrap();
    let result = std::panic::catch_unwind(|| store.get(&who()));
    drop(held);
    assert!(result.is_err());
}
