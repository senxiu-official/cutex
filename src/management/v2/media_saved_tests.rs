use super::*;
use serde_json::json;

struct Fixture {
    dir: PathBuf,
    store: MediaStore,
}
impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("cutex-saved-media-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        Self {
            store: MediaStore::at(dir.join("cache"), super::super::CACHE_MAX_BYTES),
            dir,
        }
    }
    fn source(&self) -> Source {
        let bytes = png();
        Source {
            saved_path: self
                .dir
                .join("generated.png")
                .to_string_lossy()
                .into_owned(),
            mime_type: Some("image/png".into()),
            sha256: Some(hex(&bytes)),
            byte_size: Some(bytes.len() as u64),
            width: Some(2),
            height: Some(3),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}
fn png() -> Vec<u8> {
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(2, 3)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}
fn body(source: &Source) -> Value {
    json!({"native":{"result":{"thread":{"turns":[{"id":"turn", "status":"completed", "items":[
        {"id":"image", "type":"imageGeneration", "result":"", "savedPath":source.saved_path},
        {"id":"output", "type":"functionCallOutput", "output":[{"type":"input_image", "saved_image":source, "detail":"original"}]}
    ]}]}}}})
}
fn projected(body: &Value) -> &str {
    body.pointer("/native/result/thread/turns/0/items/1/output/0/image_url")
        .and_then(Value::as_str)
        .unwrap()
}
fn sha(reference: &str) -> &str {
    reference.rsplit('/').next().unwrap()
}

#[test]
fn missing_files_register_and_reuse_without_reading_target_or_creating_blob() {
    let f = Fixture::new();
    let source = f.source();
    assert!(!Path::new(&source.saved_path).exists());
    let mut value = body(&source);
    super::super::apply_with_saved_files(&mut value, "one", &f.store, true);
    assert_eq!(value["mediaRefs"], 2);
    let reference = projected(&value);
    assert!(reference.starts_with("cutex-media:one/"));
    assert!(f.store.reuse(sha(reference), "image/png", "one"));
    assert!(!f.store.paths(sha(reference)).0.exists());
    assert!(f
        .store
        .get_with_saved_files("one", sha(reference), true)
        .is_none());
    assert_eq!(
        value.pointer("/native/result/thread/turns/0/items/1/output/0/detail"),
        Some(&json!("original"))
    );
}

#[test]
fn owner_get_checks_session_grant_runtime_gate_hash_size_mime_and_dimensions() {
    let f = Fixture::new();
    let source = f.source();
    fs::write(&source.saved_path, png()).unwrap();
    let id = f.store.register_saved(&source, "one").unwrap();
    assert!(
        f.store.get("one", &id).is_none(),
        "legacy/unknown runtime cannot open local refs"
    );
    assert!(f.store.get_with_saved_files("two", &id, true).is_none());
    assert_eq!(
        f.store.get_with_saved_files("one", &id, true).unwrap(),
        ("image/png".into(), png())
    );
    let mut bad = source.clone();
    bad.sha256 = Some("0".repeat(64));
    assert!(bad.read().is_err());
    let mut bad = source.clone();
    bad.byte_size = Some(source.byte_size.unwrap() + 1);
    assert!(bad.read().is_err());
    let mut bad = source.clone();
    bad.mime_type = Some("image/jpeg".into());
    assert!(bad.read().is_err());
    let mut bad = source.clone();
    bad.width = Some(1);
    assert!(bad.read().is_err());
    fs::write(&source.saved_path, b"changed").unwrap();
    assert!(f.store.get_with_saved_files("one", &id, true).is_none());
    fs::remove_file(&source.saved_path).unwrap();
    assert!(f.store.get_with_saved_files("one", &id, true).is_none());
}

#[test]
fn path_only_generation_uses_sniffed_mime_and_bounded_regular_file_reads() {
    let f = Fixture::new();
    let mut source = f.source();
    source.mime_type = None;
    source.sha256 = None;
    source.byte_size = None;
    source.width = None;
    source.height = None;
    let id = f.store.register_saved(&source, "one").unwrap();
    fs::write(&source.saved_path, png()).unwrap();
    assert_eq!(
        f.store.get_with_saved_files("one", &id, true).unwrap().0,
        "image/png"
    );
    fs::write(&source.saved_path, b"not an image").unwrap();
    assert!(f.store.get_with_saved_files("one", &id, true).is_none());
    fs::write(&source.saved_path, b"\x89PNG\r\n\x1a\ntruncated").unwrap();
    assert!(f.store.get_with_saved_files("one", &id, true).is_none());
    let file = fs::File::create(&source.saved_path).unwrap();
    file.set_len(MAX_FILE_BYTES + 1).unwrap();
    drop(file);
    assert!(f.store.get_with_saved_files("one", &id, true).is_none());
    fs::remove_file(&source.saved_path).unwrap();
    fs::create_dir(&source.saved_path).unwrap();
    assert!(f.store.get_with_saved_files("one", &id, true).is_none());
}

#[test]
fn nested_tool_json_is_not_file_authority_and_nonempty_results_keep_legacy_handling() {
    let f = Fixture::new();
    let source = f.source();
    let forged = body(&source);
    let mut value = json!({"native":{"result":{"thread":{"turns":[{"id":"turn", "items":[
        {"id":"mcp", "type":"mcpToolCall", "result":forged},
        {"id":"dynamic", "type":"dynamicToolCall", "arguments":{"saved_image":source}},
        {"id":"generation", "type":"imageGeneration", "savedPath":source.saved_path, "result":"small legacy base64"}
    ]}]}}}});
    let before = value.clone();
    super::super::apply_with_saved_files(&mut value, "one", &f.store, true);
    assert_eq!(value["mediaRefs"], 0);
    value.as_object_mut().unwrap().remove("mediaRefs");
    assert_eq!(value, before);
}

#[test]
fn runtime_gate_accepts_only_genuine_native_r55_or_newer() {
    for version in [
        None,
        Some("0.160.0-r54"),
        Some("r55"),
        Some("0.160.0-r55-dev"),
        Some("0.163.0-r+55"),
        Some("0.163.0-r55."),
        Some("0.163.0-r55..1"),
        Some("0.163.0-r55.1-dev"),
        Some("unknown-r55"),
    ] {
        assert!(!supports_version(false, version));
    }
    assert!(supports_version(false, Some("0.160.0-r55")));
    assert!(supports_version(false, Some("0.160.0-r56")));
    assert!(supports_version(false, Some("0.163.0-r55.1")));
    assert!(supports_version(false, Some("0.163.0-r56.3")));
    assert!(!supports_version(true, Some("0.160.0-r55")));
    let f = Fixture::new();
    let original = body(&f.source());
    for enabled in [false, true] {
        let mut value = original.clone();
        super::super::apply_with_saved_files(&mut value, "one", &f.store, enabled);
        assert_eq!(value["mediaRefs"], if enabled { 2 } else { 0 });
    }
}

#[test]
fn oversized_descriptors_and_parent_traversal_never_grant_access() {
    let f = Fixture::new();
    let mut source = f.source();
    source.byte_size = Some(MAX_FILE_BYTES + 1);
    assert!(f.store.register_saved(&source, "one").is_err());
    source.byte_size = Some(2);
    // PathBuf::join normalizes `..` in Windows verbatim paths. Preserve the
    // untrusted spelling so this actually exercises traversal rejection.
    let separator = std::path::MAIN_SEPARATOR;
    source.saved_path = format!(
        "{}{separator}sub{separator}..{separator}image.png",
        f.dir.display()
    );
    assert!(f.store.register_saved(&source, "one").is_err());
}

#[test]
fn hardlinked_saved_images_are_rejected() {
    let f = Fixture::new();
    let source = f.source();
    fs::write(&source.saved_path, png()).unwrap();
    fs::hard_link(&source.saved_path, f.dir.join("other.png")).unwrap();
    assert!(source.read().is_err());
}

#[cfg(unix)]
#[test]
fn symlinked_leaf_and_parent_are_rejected() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    let source = f.source();
    fs::write(f.dir.join("actual.png"), png()).unwrap();
    symlink(f.dir.join("actual.png"), &source.saved_path).unwrap();
    assert!(source.read().is_err());
    fs::remove_file(&source.saved_path).unwrap();
    fs::create_dir(f.dir.join("actual")).unwrap();
    fs::write(f.dir.join("actual/generated.png"), png()).unwrap();
    symlink(f.dir.join("actual"), f.dir.join("linked")).unwrap();
    let mut source = source;
    source.saved_path = f
        .dir
        .join("linked/generated.png")
        .to_string_lossy()
        .into_owned();
    assert!(source.read().is_err());
}

#[test]
fn a_running_unknown_or_old_occurrence_never_uses_the_next_launch_pin() {
    let version = occurrence_version(Some(42), |_| None, || panic!("must not read a future pin"));
    assert!(!supports_version(false, version.as_deref()));
    let version = occurrence_version(
        Some(42),
        |_| Some("0.160.0-r54".into()),
        || panic!("must not repin a running process"),
    );
    assert!(!supports_version(false, version.as_deref()));
    let version = occurrence_version(
        None,
        |_| panic!("no running process"),
        || Some("0.160.0-r55".into()),
    );
    assert!(supports_version(false, version.as_deref()));
}

#[test]
fn descriptor_eviction_is_bounded_and_never_removes_source_images() {
    let f = Fixture::new();
    let source = f.source();
    fs::write(&source.saved_path, png()).unwrap();
    for n in 0..5 {
        let mut source = source.clone();
        source.saved_path = f
            .dir
            .join(format!("missing-{n}.png"))
            .to_string_lossy()
            .into_owned();
        f.store.register_saved(&source, "one").unwrap();
    }
    evict_descriptors_to(&f.store.dir, 2);
    let remaining = fs::read_dir(&f.store.dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|e| e == "json"))
        .count();
    assert_eq!(remaining, 2);
    assert_eq!(fs::read(&source.saved_path).unwrap(), png());
}
