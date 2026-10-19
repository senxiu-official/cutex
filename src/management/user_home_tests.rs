use super::*;
use std::io::Cursor;

fn png(width: u32, height: u32) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(width, height).write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

fn upload(bytes: Vec<u8>) -> AvatarUpload {
    AvatarUpload { content_type: "image/png".into(), bytes }
}

struct Temp(PathBuf);

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp() -> (Temp, HomeStore) {
    let root = std::env::temp_dir().join(format!("cutex-user-home-{}", uuid::Uuid::new_v4()));
    let store = HomeStore::at(root.join("users"));
    (Temp(root), store)
}

fn home(value: Value) -> Option<Map<String, Value>> {
    validate_home(&value).unwrap()
}

#[test]
fn empty_put_get_and_revision_conflicts() {
    let (_temp, store) = temp();
    let empty = store.get("owner").unwrap();
    assert_eq!(empty.view(), json!({"revision": 0, "home": null, "coverEtag": null, "updatedAt": null}));

    let first = json!({"layout": "card", "bio": "hi", "future": {"x": 1}});
    let record = store.put("owner", 0, home(first.clone()), "root").unwrap();
    assert_eq!(record.revision, 1);
    let view = store.get("owner").unwrap().view();
    assert_eq!(view["revision"], 1);
    assert_eq!(view["home"]["future"], json!({"x": 1}), "unknown keys are kept");
    assert_eq!(view["home"]["coverEtag"], Value::Null);
    assert!(view["updatedAt"].is_string());

    // Unchanged: same revision, no conflict on the current one.
    assert_eq!(store.put("owner", 1, home(first), "root").unwrap().revision, 1);
    // Stale revision → 409 with the current revision.
    let error = store.put("owner", 0, home(json!({"layout": "gallery"})), "root").unwrap_err();
    assert_eq!((error.status, error.code, &error.details), (409, "home_revision_conflict", &json!({"revision": 1})));
    // Clearing is a change.
    let record = store.put("owner", 1, None, "root").unwrap();
    assert_eq!((record.revision, record.home.is_none()), (2, true));
    // Files are private and per user.
    assert!(store.dir().join("owner.json").is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(store.dir().join("owner.json")).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
    }
}

#[test]
fn users_are_isolated_and_ids_checked() {
    let (_temp, store) = temp();
    store.put("owner", 0, home(json!({"bio": "owner"})), "root").unwrap();
    store.put("u-0123456789ab", 0, home(json!({"bio": "member"})), "root").unwrap();
    assert_eq!(store.get("owner").unwrap().home.unwrap()["bio"], "owner");
    assert_eq!(store.get("u-0123456789ab").unwrap().home.unwrap()["bio"], "member");
    assert_eq!(store.get("u-ba9876543210").unwrap().revision, 0);
    for bad in ["../owner", "u-XYZ", "", "owner.json"] {
        assert_eq!(store.get(bad).unwrap_err().code, "user_not_found", "{bad}");
        assert!(store.put(bad, 0, None, "root").is_err());
    }
    store.remove("u-0123456789ab");
    assert_eq!(store.get("u-0123456789ab").unwrap(), Record::default());
    assert_eq!(store.get("owner").unwrap().revision, 1);
}

#[test]
fn home_shape_and_size_rules() {
    let full = json!({
        "layout": "card",
        "blocks": [{"id": "pins", "visible": true}, {"id": "stats", "visible": false, "extra": 1}],
        "bio": "line one\nline two",
        "sidebarEntry": true,
        "showWhenEmpty": false,
        "homeHost": "host-a",
        "pins": [
            {"hostId": "host-a", "kind": "service", "key": "web"},
            {"hostId": "host-b", "kind": "url", "key": "https://example.com", "title": "Example", "size": "w"},
            {"hostId": "host-b", "kind": "agent", "key": "cutex.a", "title": null, "size": null},
        ],
        "coverEtag": "client-supplied",
        "unknownTopLevel": [1, 2, 3],
    });
    let stored = validate_home(&full).unwrap().unwrap();
    assert!(!stored.contains_key("coverEtag"), "coverEtag is server-owned");
    assert!(stored.contains_key("unknownTopLevel"));
    assert_eq!(validate_home(&Value::Null).unwrap(), None);
    assert!(validate_home(&json!({})).unwrap().unwrap().is_empty());

    let pin = |extra: Value| {
        let mut pin = json!({"hostId": "h", "kind": "service", "key": "k"});
        pin.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        json!({"pins": [pin]})
    };
    let too_many_pins: Vec<Value> = (0..=MAX_PINS).map(|i| json!({"hostId": "h", "kind": "url", "key": format!("k{i}")})).collect();
    for bad in [
        json!([]),
        json!("home"),
        json!({"layout": 3}),
        json!({"layout": ""}),
        json!({"bio": "x".repeat(MAX_BIO_CHARS + 1)}),
        json!({"bio": "bell\u{7}"}),
        json!({"sidebarEntry": "yes"}),
        json!({"showWhenEmpty": 1}),
        json!({"homeHost": 7}),
        json!({"blocks": {}}),
        json!({"blocks": [{"id": "a"}]}),
        json!({"blocks": [{"visible": true}]}),
        json!({"blocks": [{"id": "a", "visible": true}, {"id": "a", "visible": false}]}),
        json!({"blocks": ["a"]}),
        json!({"pins": "none"}),
        json!({"pins": too_many_pins}),
        json!({"pins": [{"kind": "service", "key": "k"}]}),
        pin(json!({"kind": "folder"})),
        pin(json!({"key": ""})),
        pin(json!({"size": "xl"})),
        pin(json!({"title": "t".repeat(201)})),
    ] {
        let error = validate_home(&bad).unwrap_err();
        assert_eq!((error.status, error.code), (400, "invalid_home"), "{bad}");
    }
    assert!(validate_home(&json!({"bio": "x".repeat(MAX_BIO_CHARS)})).is_ok(), "200 characters fit");
    assert!(validate_home(&json!({"bio": "字".repeat(MAX_BIO_CHARS)})).is_ok(), "characters, not bytes");

    let big = json!({"notes": "x".repeat(MAX_HOME_BYTES)});
    let error = validate_home(&big).unwrap_err();
    assert_eq!((error.status, error.code), (413, "home_too_large"));
    let fits = json!({"n": "x".repeat(MAX_HOME_BYTES - 8)});
    assert_eq!(serde_json::to_vec(&fits).unwrap().len(), MAX_HOME_BYTES);
    assert!(validate_home(&fits).is_ok());
}

#[test]
fn cover_set_replace_clear_and_limits() {
    let (_temp, store) = temp();
    assert!(store.cover("owner").is_none());
    let record = store.set_cover("owner", Some(&upload(png(1200, 400))), "root").unwrap();
    let first = record.cover_etag().unwrap().to_string();
    assert_eq!(record.revision, 0, "cover changes do not move the revision");
    let (cover, bytes) = store.cover("owner").unwrap();
    assert_eq!((cover.width, cover.height, bytes), (1200, 400, png(1200, 400)));
    // The view carries the etag (home null: only the top-level one).
    assert_eq!(record.view()["coverEtag"], json!(first));
    store.put("owner", 0, home(json!({"layout": "card"})), "root").unwrap();
    assert_eq!(store.get("owner").unwrap().view()["home"]["coverEtag"], json!(first));

    let record = store.set_cover("owner", Some(&upload(png(800, 200))), "root").unwrap();
    let second = record.cover_etag().unwrap().to_string();
    assert_ne!(first, second);
    let files = |store: &HomeStore| {
        std::fs::read_dir(store.dir()).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.contains("-cover-")).collect::<Vec<_>>()
    };
    assert_eq!(files(&store), vec![format!("owner-cover-{second}.png")], "the replaced image is removed");

    let code = |bytes: Vec<u8>, content_type: &str| {
        store.set_cover("owner", Some(&AvatarUpload { content_type: content_type.into(), bytes }), "root").unwrap_err().code
    };
    assert_eq!(code(png(MAX_COVER_SIDE + 1, 10), "image/png"), "invalid_cover");
    assert_eq!(code(png(10, 10), "image/jpeg"), "invalid_cover");
    assert_eq!(code(b"GIF89a....".to_vec(), "image/gif"), "invalid_cover");
    assert_eq!(code(vec![0; MAX_COVER_BYTES + 1], "image/png"), "cover_too_large");
    assert_eq!(store.get("owner").unwrap().cover_etag(), Some(second.as_str()), "refused uploads change nothing");

    let record = store.set_cover("owner", None, "root").unwrap();
    assert_eq!(record.cover_etag(), None);
    assert!(files(&store).is_empty());
    assert!(store.cover("owner").is_none());
    // Clearing again is a no-op.
    assert!(store.set_cover("owner", None, "root").is_ok());
}

#[test]
fn cover_uploads_parse_with_the_cover_limit() {
    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD.encode(vec![0u8; MAX_COVER_BYTES + 64]);
    let error = parse_cover_upload(&json!({"contentType": "image/png", "data": data})).unwrap_err();
    assert_eq!((error.status, error.code), (413, "cover_too_large"));
    let data = base64::engine::general_purpose::STANDARD.encode(png(300, 100));
    assert_eq!(parse_cover_upload(&json!({"contentType": "image/png", "data": data})).unwrap().bytes, png(300, 100));
    assert_eq!(parse_cover_upload(&json!({"data": "x"})).unwrap_err().code, "invalid_cover");
}
