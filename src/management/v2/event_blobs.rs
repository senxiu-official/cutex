//! Durable binary payloads for native events. Unlike history-media's cache,
//! these blobs back retained events and are not evicted on a timer or LRU.
use anyhow::Context;
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
#[path = "event_blob_fs.rs"]
mod blob_fs;
use std::{
    io::{self, Write},
    path::PathBuf,
};

pub const MAX_EVENT_BYTES: usize = 256 * 1024;
const PAYLOAD_BYTES: usize = MAX_EVENT_BYTES - 16 * 1024;
const INLINE_BYTES: usize = 32 * 1024;
const MAX_BLOB_BYTES: usize = 32 * 1024 * 1024;

pub struct EventBlobStore {
    dir: PathBuf,
}

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn valid_sha(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn parse_path(path: &str) -> Option<(String, String)> {
    let (session, id) = path.strip_prefix("/v2/sessions/")?.split_once("/blobs/")?;
    let session = super::server::decode_path_segment(session)?;
    valid_sha(id).then(|| (session, id.to_owned()))
}

pub(crate) fn encoded_len<T: serde::Serialize>(value: &T) -> usize {
    struct Counter(usize);
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    let _ = serde_json::to_writer(&mut counter, value);
    counter.0
}

impl EventBlobStore {
    pub fn open_default() -> anyhow::Result<Self> {
        Ok(Self::at(
            crate::config::paths::runtime_dir()?.join("management-v2/event-blobs"),
        ))
    }
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn grant_path(&self, session: &str, id: &str) -> PathBuf {
        let grant = format!(
            "{:x}",
            Sha256::new()
                .chain_update(session.as_bytes())
                .chain_update([0])
                .chain_update(id.as_bytes())
                .finalize()
        );
        self.dir.join("grants").join(format!("{grant}.json"))
    }

    pub fn put(&self, session: &str, mime: &str, bytes: &[u8]) -> anyhow::Result<Value> {
        anyhow::ensure!(
            bytes.len() <= MAX_BLOB_BYTES,
            "event blob exceeds byte limit"
        );
        anyhow::ensure!(!session.is_empty(), "event blob needs a session");
        anyhow::ensure!(
            mime.len() < 128
                && mime
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"/-+.;=".contains(&b)),
            "invalid event blob MIME"
        );
        let id = sha(bytes);
        // Immutable content, atomic publication: concurrent writers of the same
        // hash write identical bytes. Publish the grant only after the bytes.
        blob_fs::write_once(&self.dir, &format!("{id}.bin"), bytes)?;
        let grant = self.grant_path(session, &id);
        let grant_dir = grant.parent().expect("constructed grant directory");
        let grant_name = grant
            .file_name()
            .and_then(|s| s.to_str())
            .expect("hex grant filename");
        let dimensions = super::image_dimensions::dimensions(bytes);
        let mut metadata = json!({"mime":mime,"size":bytes.len()});
        add_dimensions(&mut metadata, dimensions);
        let metadata = serde_json::to_vec(&metadata)?;
        let mime = match blob_fs::write_once(grant_dir, grant_name, &metadata) {
            Ok(()) => mime.to_owned(),
            Err(error) => {
                // Equivalent MIME aliases keep the first immutable grant.
                let previous = blob_fs::read(grant_dir, grant_name, 4096)
                    .ok()
                    .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok());
                match previous
                    .filter(|meta| meta["size"].as_u64() == Some(bytes.len() as u64))
                    .and_then(|meta| {
                        meta["mime"]
                            .as_str()
                            .filter(|mime| valid_mime(mime))
                            .map(str::to_owned)
                    }) {
                    Some(mime) => mime,
                    None => return Err(error),
                }
            }
        };
        let mut descriptor = json!({"type":"cutexBlob","blobId":id,"sha256":id,"mime":mime,
            "size":bytes.len(),"url":format!("cutex-blob:{session}/{id}")});
        add_dimensions(&mut descriptor, dimensions);
        Ok(descriptor)
    }

    pub fn get(&self, session: &str, id: &str) -> Option<(String, Vec<u8>)> {
        if !valid_sha(id) {
            return None;
        }
        let grant = self.grant_path(session, id);
        let meta: Value = serde_json::from_slice(
            &blob_fs::read(grant.parent()?, grant.file_name()?.to_str()?, 4096).ok()?,
        )
        .ok()?;
        let size = meta["size"].as_u64()?;
        if size > MAX_BLOB_BYTES as u64 {
            return None;
        }
        let bytes = blob_fs::read(&self.dir, &format!("{id}.bin"), size as usize).ok()?;
        if bytes.len() != size as usize {
            return None;
        }
        if sha(&bytes) != id {
            return None;
        }
        let mime = meta["mime"].as_str()?;
        if !valid_mime(mime) {
            return None;
        }
        Some((mime.to_string(), bytes))
    }

    /// Walk before cloning/serializing/fan-out. Large non-media values are
    /// explicitly bounded too; lifecycle identities and status remain intact.
    pub fn normalize(&self, session: &str, value: &mut Value) -> anyhow::Result<()> {
        self.normalize_with(session, value, true)
    }

    /// Cutex event params have a strict method schema. Only typed media fields
    /// may change from strings into blob descriptors; an arbitrary string that
    /// happens to contain a data URL must keep its schema-defined type.
    pub fn normalize_cutex(&self, session: &str, value: &mut Value) -> anyhow::Result<()> {
        self.normalize_with(session, value, false)
    }

    fn normalize_with(&self, session: &str, value: &mut Value, generic_data_urls: bool) -> anyhow::Result<()> {
        self.media(session, value, 0, generic_data_urls)?;
        if encoded_fits(value, PAYLOAD_BYTES) {
            return Ok(());
        }
        let full_text_unavailable = self.preserve_completed_item(session, value)?;
        let mut budget = PAYLOAD_BYTES / 2;
        shrink(value, &mut budget, 0);
        if let Some(map) = value.as_object_mut() {
            let mut truncated = json!({"reason":"event_size_limit","maxBytes":MAX_EVENT_BYTES,"resyncRequired":true});
            if let Some(reason) = full_text_unavailable {
                truncated["fullTextUnavailable"] = json!(reason);
                truncated["repair"] = json!("thread/read");
            }
            map.insert("_cutexTruncated".into(), truncated);
        }
        anyhow::ensure!(
            encoded_fits(value, PAYLOAD_BYTES),
            "event metadata exceeds bounded payload limit"
        );
        Ok(())
    }

    /// The inline completion remains bounded, while its complete item (after
    /// media externalization) is recoverable through the existing session grant.
    /// Do not turn a replayed, already shortened item into an authoritative blob.
    fn preserve_completed_item(&self, session: &str, value: &mut Value) -> anyhow::Result<Option<&'static str>> {
        if value.get("method").and_then(Value::as_str) != Some("item/completed") {
            return Ok(None);
        }
        let Some(item) = value.pointer("/params/item") else { return Ok(None) };
        if !matches!(item.get("type").and_then(Value::as_str), Some("agentMessage" | "reasoning")) {
            return Ok(None);
        }
        if value.get("_cutexFullText").is_some_and(|descriptor| {
            let Some(id) = descriptor.get("blobId").and_then(Value::as_str).filter(|id| valid_sha(id)) else { return false };
            descriptor["type"] == "cutexBlob" && descriptor["mime"] == "application/json"
                && descriptor["sha256"] == id
                && descriptor["url"] == format!("cutex-blob:{session}/{id}")
                && descriptor["size"].as_u64().is_some_and(|size| size <= MAX_BLOB_BYTES as u64)
                && encoded_fits(descriptor, 4096)
        }) {
            return Ok(None);
        }
        value.as_object_mut().expect("notification object").remove("_cutexFullText");
        if value.get("_cutexTruncated").is_some() {
            return Ok(Some(if value.pointer("/_cutexTruncated/fullTextUnavailable").and_then(Value::as_str) == Some("blob_size_limit") {
                "blob_size_limit"
            } else { "source_already_truncated" }));
        }
        let item = value.pointer("/params/item").expect("checked item");
        // Counting serialization stops at the cap; do not allocate a second
        // unbounded JSON copy just to discover that it cannot be stored.
        if !encoded_fits(item, MAX_BLOB_BYTES) {
            return Ok(Some("blob_size_limit"));
        }
        let bytes = serde_json::to_vec(item)?;
        let descriptor = self.put(session, "application/json", &bytes)?;
        if descriptor["mime"] != "application/json" {
            // Existing immutable grants preserve their first MIME. Do not
            // advertise a JSON repair reference that the client must reject.
            return Ok(Some("blob_mime_conflict"));
        }
        if !encoded_fits(&descriptor, 4096) {
            return Ok(Some("blob_reference_limit"));
        }
        value["_cutexFullText"] = descriptor;
        Ok(None)
    }

    /// History pages / RPC responses are not single events: keep their full
    /// text and pagination, but never copy inline binary media into receipts.
    pub fn externalize_media(&self, session: &str, value: &mut Value) -> anyhow::Result<()> {
        self.media(session, value, 0, true)
    }

    fn media(&self, session: &str, value: &mut Value, depth: usize, generic_data_urls: bool) -> anyhow::Result<()> {
        if depth > 96 {
            return Ok(());
        }
        if let Some(text) = value.as_str().filter(|s| generic_data_urls && s.len() > INLINE_BYTES) {
            if let Some((mime, data)) = text
                .strip_prefix("data:")
                .and_then(|s| s.split_once(";base64,"))
            {
                if mime.len() < 128
                    && (mime.starts_with("image/")
                        || mime.starts_with("audio/")
                        || mime == "application/pdf")
                {
                    *value = self.reference(session, mime, data)?;
                    return Ok(());
                }
            }
        }
        match value {
            Value::Object(map) => {
                if map.get("type").and_then(Value::as_str) == Some("cutexBlob") {
                    let valid = map
                        .get("blobId")
                        .and_then(Value::as_str)
                        .filter(|id| valid_sha(id))
                        .is_some_and(|id| {
                            map.get("sha256").and_then(Value::as_str) == Some(id)
                                && map.get("url").and_then(Value::as_str)
                                    == Some(format!("cutex-blob:{session}/{id}").as_str())
                                && map
                                    .get("mime")
                                    .and_then(Value::as_str)
                                    .is_some_and(valid_mime)
                                && map
                                    .get("size")
                                    .and_then(Value::as_u64)
                                    .is_some_and(|size| size <= MAX_BLOB_BYTES as u64)
                        });
                    if !valid {
                        *value = json!({"type":"imageNotLoaded","truncated":true,"reason":"invalid_blob_reference"});
                    }
                    return Ok(());
                }
                let generated = map.get("type").and_then(Value::as_str) == Some("imageGeneration");
                let media = matches!(
                    map.get("type").and_then(Value::as_str),
                    Some("image" | "audio" | "inputImage")
                );
                let mime = map
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .filter(|s| s.len() < 128)
                    .map(str::to_owned);
                for key in [
                    "result",
                    "data",
                    "url",
                    "base64",
                    "imageData",
                    "b64_json",
                    "image_base64",
                ] {
                    let typed_media_url = media && key == "url";
                    if (generated && key == "result")
                        || (media && key == "data")
                        || typed_media_url
                        || matches!(key, "base64" | "imageData" | "b64_json" | "image_base64")
                    {
                        if let Some(text) = map
                            .get(key)
                            .and_then(Value::as_str)
                            .filter(|s| s.len() > INLINE_BYTES)
                        {
                            if let Some((data_mime, data)) = text
                                .strip_prefix("data:")
                                .and_then(|s| s.split_once(";base64,"))
                                .filter(|(data_mime, _)| data_mime.len() < 128 &&
                                    (data_mime.starts_with("image/") || data_mime.starts_with("audio/") || *data_mime == "application/pdf"))
                            {
                                map.insert(key.into(), self.reference(session, data_mime, data)?);
                            } else if !typed_media_url {
                                let blob = self.reference(
                                    session,
                                    mime.as_deref().unwrap_or(if generated || media {
                                        "image/png"
                                    } else {
                                        "application/octet-stream"
                                    }),
                                    text,
                                )?;
                                map.insert(key.into(), blob);
                            }
                        }
                    }
                }
                for child in map.values_mut() {
                    self.media(session, child, depth + 1, generic_data_urls)?;
                }
            }
            Value::Array(items) => {
                for child in items {
                    self.media(session, child, depth + 1, generic_data_urls)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn reference(&self, session: &str, mime: &str, text: &str) -> anyhow::Result<Value> {
        let mime = mime.split(';').next().unwrap_or("").trim();
        if mime.is_empty()
            || mime.len() >= 128
            || !mime
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/-+.".contains(&b))
        {
            return Ok(json!({"type":"imageNotLoaded","truncated":true,"reason":"invalid_mime"}));
        }
        if text.len() > MAX_BLOB_BYTES * 4 / 3 + 4 {
            return Ok(
                json!({"type":"imageNotLoaded","truncated":true,"reason":"blob_size_limit"}),
            );
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(text.len() / 4 * 3 + 3)
            .context("reserve event blob decode buffer")?;
        match base64::engine::general_purpose::STANDARD.decode_vec(text.trim(), &mut bytes) {
            Ok(()) => {
                if bytes.len() > MAX_BLOB_BYTES {
                    return Ok(
                        json!({"type":"imageNotLoaded","truncated":true,"reason":"blob_size_limit"}),
                    );
                }
                let mime = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
                    "image/png"
                } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
                    "image/jpeg"
                } else if bytes.starts_with(b"GIF8") {
                    "image/gif"
                } else if bytes.len() > 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP"
                {
                    "image/webp"
                } else {
                    mime
                };
                self.put(session, mime, &bytes)
                    .context("persist native event image")
            }
            Err(_) => {
                Ok(json!({"type":"imageNotLoaded","truncated":true,"reason":"invalid_base64"}))
            }
        }
    }
}

fn add_dimensions(value: &mut Value, dimensions: Option<(u32, u32)>) {
    if let Some((width, height)) = dimensions {
        value["width"] = width.into();
        value["height"] = height.into();
    }
}

fn valid_mime(mime: &str) -> bool {
    !mime.is_empty()
        && mime.len() < 128
        && mime
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/-+.;=".contains(&b))
}

/// serde's counting writer stops at the budget instead of allocating another
/// multi-megabyte serialized copy just to discover its size.
pub(crate) fn encoded_fits<T: serde::Serialize>(value: &T, max: usize) -> bool {
    struct Counter(usize);
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.0 {
                return Err(io::Error::other("size limit"));
            }
            self.0 -= bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Counter(max), value).is_ok()
}

fn shrink(value: &mut Value, budget: &mut usize, depth: usize) {
    if value.get("type").and_then(Value::as_str) == Some("cutexBlob") {
        if encoded_fits(value, (*budget).min(4096)) {
            *budget = budget.saturating_sub(encoded_len(value));
        } else {
            *value = json!({"type":"imageNotLoaded","truncated":true,"reason":"event_size_limit"});
        }
        return;
    }
    if depth > 64 {
        *value = json!({"truncated":true});
        return;
    }
    match value {
        Value::String(s) => {
            let max = (*budget / 6).min(8192);
            if s.len() > max {
                let end = super::compact::cut_utf8(s, max).len();
                s.truncate(end);
                s.push_str(" [truncated]");
            }
            *budget = budget.saturating_sub(s.len() * 6 + 16);
        }
        Value::Array(a) => {
            let original = a.len();
            let mut keep = 0;
            for v in a.iter_mut() {
                if *budget < 64 {
                    break;
                }
                shrink(v, budget, depth + 1);
                keep += 1;
            }
            a.truncate(keep);
            if keep < original {
                a.push(json!({"truncated":true,"omittedItems":original-keep}));
            }
        }
        Value::Object(map) => {
            // Preserve identities/status before consuming the budget on prose.
            let mut next = serde_json::Map::new();
            // Only a small root-level repair descriptor is protected. A tool
            // result may contain an arbitrary identically named value; treating
            // that value as an identity would bypass ordinary payload shrinking.
            if depth == 0 && map.get("_cutexFullText").is_some_and(|reference| {
                reference["type"] == "cutexBlob" && reference["mime"] == "application/json"
                    && encoded_fits(reference, 4096)
            }) {
                let reference = map.remove("_cutexFullText").expect("checked repair reference");
                *budget = budget.saturating_sub(encoded_len(&reference) + 32);
                next.insert("_cutexFullText".into(), reference);
            }
            for key in [
                "method",
                "id",
                "type",
                "status",
                "threadId",
                "turnId",
                "itemId",
                "thread_id",
                "turn_id",
                "item_id",
                "blobId",
                "sha256",
                "mime",
                "size",
                "url",
            ] {
                if let Some(v) = map.remove(key) {
                    // Never change an identity into a different identity. If
                    // identities alone exceed the envelope budget, normalize
                    // fails explicitly instead of publishing a false event.
                    *budget = budget.saturating_sub(encoded_len(&v) + key.len() + 8);
                    next.insert(key.into(), v);
                }
            }
            for key in ["params", "turn", "item", "thread"] {
                if let Some(mut v) = map.remove(key) {
                    shrink(&mut v, budget, depth + 1);
                    next.insert(key.into(), v);
                }
            }
            let mut omitted = 0;
            for (key, mut v) in std::mem::take(map) {
                if *budget < 64 || key.len() > 512 {
                    omitted += 1;
                    continue;
                }
                *budget = budget.saturating_sub(key.len() * 6 + 16);
                shrink(&mut v, budget, depth + 1);
                next.insert(key, v);
            }
            if omitted > 0 {
                next.insert("_cutexOmittedFields".into(), json!(omitted));
            }
            *map = next;
        }
        _ => *budget = budget.saturating_sub(32),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn png() -> Vec<u8> {
        base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aWQAAAABJRU5ErkJggg==").unwrap()
    }

    #[test]
    fn oversized_completed_text_roundtrips_full_json_and_preserves_reference_on_replay() {
        let dir = std::env::temp_dir().join(format!("eb-full-{}", uuid::Uuid::new_v4()));
        let store = EventBlobStore::at(&dir);
        let text = "汉🙂完整".repeat(40_000);
        for item in [
            json!({"id":"message","type":"agentMessage","text":text,"phase":"final_answer","future":{"keep":true}}),
            json!({"id":"reason","type":"reasoning","summary":[text,"第二段"],"content":["第一部分",text],"future":{"keep":true}}),
        ] {
            let mut notification = json!({"method":"item/completed","params":{"threadId":"thread-1","turnId":"turn-1","item":item}});
            store.normalize("cutex.a", &mut notification).unwrap();
            assert!(encoded_fits(&notification, PAYLOAD_BYTES));
            assert_eq!(notification["_cutexTruncated"]["resyncRequired"], true);
            assert_ne!(notification["params"]["item"], item);
            let descriptor = notification["_cutexFullText"].clone();
            assert_eq!(descriptor["type"], "cutexBlob");
            assert_eq!(descriptor["mime"], "application/json");
            let id = descriptor["blobId"].as_str().unwrap();
            assert_eq!(descriptor["sha256"], id);
            assert_eq!(descriptor["url"], format!("cutex-blob:cutex.a/{id}"));
            let (mime, bytes) = store.get("cutex.a", id).unwrap();
            assert_eq!(mime, "application/json");
            assert_eq!(descriptor["size"], bytes.len() as u64);
            assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), item);
            assert!(store.get("cutex.b", id).is_none(), "full text remains session-granted");
            // Force a second shrink, not just the already-small fast path.
            notification["extra"] = json!("x".repeat(PAYLOAD_BYTES * 2));
            store.normalize("cutex.a", &mut notification).unwrap();
            assert_eq!(notification["_cutexFullText"], descriptor);
            assert!(encoded_fits(&notification, PAYLOAD_BYTES));
            assert_eq!(serde_json::from_slice::<Value>(&store.get("cutex.a", id).unwrap().1).unwrap(), item);
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn oversized_completed_text_above_blob_cap_explicitly_requires_history() {
        let dir = std::env::temp_dir().join(format!("eb-cap-{}", uuid::Uuid::new_v4()));
        let store = EventBlobStore::at(&dir);
        let mut notification = json!({"method":"item/completed","params":{"threadId":"t","item":{"id":"m","type":"agentMessage","text":"x".repeat(MAX_BLOB_BYTES + 1)}}});
        store.normalize("cutex.a", &mut notification).unwrap();
        assert!(notification.get("_cutexFullText").is_none());
        assert_eq!(notification["_cutexTruncated"]["fullTextUnavailable"], "blob_size_limit");
        assert_eq!(notification["_cutexTruncated"]["repair"], "thread/read");
        assert!(encoded_fits(&notification, PAYLOAD_BYTES));
        assert!(!dir.exists(), "oversized repair JSON must not create an oversized blob");
    }

    #[test]
    fn completed_text_already_truncated_is_never_promoted_to_full_authority() {
        let dir = std::env::temp_dir().join(format!("eb-preview-{}", uuid::Uuid::new_v4()));
        let store = EventBlobStore::at(&dir);
        let mut notification = json!({"method":"item/completed","params":{"threadId":"t","item":{"id":"m","type":"agentMessage","text":"preview [truncated]"}},"_cutexTruncated":{"resyncRequired":true},"extra":"x".repeat(PAYLOAD_BYTES * 2)});
        store.normalize("cutex.a", &mut notification).unwrap();
        assert!(notification.get("_cutexFullText").is_none());
        assert_eq!(notification["_cutexTruncated"]["fullTextUnavailable"], "source_already_truncated");
        assert_eq!(notification["_cutexTruncated"]["repair"], "thread/read");
        assert!(!dir.exists());
    }

    #[test]
    fn untrusted_same_named_full_text_values_cannot_bypass_the_event_budget() {
        let dir = std::env::temp_dir().join(format!("eb-untrusted-ref-{}", uuid::Uuid::new_v4()));
        let store = EventBlobStore::at(&dir);
        for untrusted in [json!("x".repeat(PAYLOAD_BYTES * 2)),
            json!({"type":"cutexBlob","blobId":"a".repeat(64),"sha256":"a".repeat(64),
                "mime":"application/json","size":1,"url":format!("cutex-blob:cutex.a/{}","a".repeat(64)),"extra":"x".repeat(PAYLOAD_BYTES * 2)})] {
            let mut raw = json!({"method":"future/native/event","params":{"threadId":"safe-thread","_cutexFullText":untrusted},"_cutexFullText":untrusted});
            store.normalize("cutex.a", &mut raw).unwrap();
            assert!(encoded_fits(&raw, PAYLOAD_BYTES));
            assert_eq!(raw["method"], "future/native/event");
            assert_eq!(raw["params"]["threadId"], "safe-thread");
            assert_eq!(raw["_cutexTruncated"]["resyncRequired"], true);
        }
        assert!(!dir.exists());
    }

    #[test]
    fn dimensions_are_optional_layout_hints_in_descriptor_and_grant() {
        let dir = std::env::temp_dir().join(format!("ebd-{}", uuid::Uuid::new_v4()));
        let store = EventBlobStore::at(&dir);
        let bytes = png();
        let descriptor = store.put("cutex.a", "image/png", &bytes).unwrap();
        assert_eq!(descriptor["width"], 1);
        assert_eq!(descriptor["height"], 1);
        let grant: Value = serde_json::from_slice(&fs::read(store.grant_path("cutex.a", descriptor["blobId"].as_str().unwrap())).unwrap()).unwrap();
        assert_eq!(grant["width"], 1);
        assert_eq!(grant["height"], 1);
        assert_eq!(store.get("cutex.a", descriptor["blobId"].as_str().unwrap()).unwrap().1, bytes);
        let unknown = store.put("cutex.a", "image/png", b"broken image").unwrap();
        assert!(unknown.get("width").is_none());
        assert!(unknown.get("height").is_none());
        assert!(store.get("cutex.a", unknown["blobId"].as_str().unwrap()).is_some());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn old_inline_replay_adds_dimensions_without_rewriting_legacy_grants() {
        let dir = std::env::temp_dir().join(format!("ebr-{}", uuid::Uuid::new_v4()));
        let store = EventBlobStore::at(&dir);
        let mut bytes = png();
        bytes.resize(65536, 0);
        let id = sha(&bytes);
        let grant = store.grant_path("cutex.a", &id);
        let legacy = serde_json::to_vec(&json!({"mime":"image/png","size":bytes.len()})).unwrap();
        blob_fs::write_once(grant.parent().unwrap(), grant.file_name().unwrap().to_str().unwrap(), &legacy).unwrap();
        let mut event = json!({"type":"imageGeneration","result":base64::engine::general_purpose::STANDARD.encode(&bytes)});
        store.normalize("cutex.a", &mut event).unwrap();
        assert_eq!(event["result"]["width"], 1);
        assert_eq!(event["result"]["height"], 1);
        let first = event.clone();
        store.normalize("cutex.a", &mut event).unwrap();
        assert_eq!(event, first);
        assert_eq!(fs::read(grant).unwrap(), legacy);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn large_images_are_deduplicated_bounded_and_session_scoped() {
        let dir = std::env::temp_dir().join(format!("cutex-event-blobs-{}", uuid::Uuid::new_v4()));
        let store = EventBlobStore::at(&dir);
        let bytes = vec![42; 3 * 1024 * 1024];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        for i in 0..32 {
            let mut event = json!({"method":"item/completed","params":{"threadId":"t","turnId":"turn","item":{"id":format!("i{i}"),"type":"imageGeneration","status":"completed","result":b64}}});
            store.normalize("cutex.a", &mut event).unwrap();
            assert!(serde_json::to_vec(&event).unwrap().len() < 2048);
            let r = &event["params"]["item"]["result"];
            assert_eq!(r["size"], bytes.len());
            assert!(store
                .get("cutex.b", r["blobId"].as_str().unwrap())
                .is_none());
            assert_eq!(
                store
                    .get("cutex.a", r["blobId"].as_str().unwrap())
                    .unwrap()
                    .1,
                bytes
            );
        }
        assert_eq!(
            fs::read_dir(&dir)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|e| e.path().extension().is_some_and(|x| x == "bin"))
                .count(),
            1
        );
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn data_urls_and_unknown_large_fields_are_bounded_without_losing_lifecycle() {
        let dir = std::env::temp_dir().join(format!("cutex-event-blobs-{}", uuid::Uuid::new_v4()));
        let store = EventBlobStore::at(&dir);
        let mut event = json!({"method":"turn/completed","params":{"turn":{"id":"turn1","status":"completed","items":[{"type":"image","imageUrl":format!("data:image/png;base64,{}",base64::engine::general_purpose::STANDARD.encode(vec![7;65536]))}]},"other":"x".repeat(2_000_000)}});
        store.normalize("cutex.a", &mut event).unwrap();
        assert!(encoded_fits(&event, PAYLOAD_BYTES));
        assert_eq!(event["method"], "turn/completed");
        assert_eq!(event["params"]["turn"]["id"], "turn1");
        assert_eq!(event["params"]["turn"]["status"], "completed");
        assert_eq!(event["_cutexTruncated"]["reason"], "event_size_limit");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cutex_normalization_only_rewrites_typed_media_data_urls() {
        let dir = std::env::temp_dir().join(format!("cutex-event-blobs-typed-{}", uuid::Uuid::new_v4()));
        let store = EventBlobStore::at(&dir);
        let data_url = format!("data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(vec![7;65_537]));
        let mut params = json!({
            "content": data_url.clone(),
            "input": [{"type":"image", "url":data_url}]
        });
        store.normalize_cutex("cutex.a", &mut params).unwrap();
        assert!(params["content"].is_string());
        assert_eq!(params["input"][0]["url"]["type"], "cutexBlob");
        assert!(store.get("cutex.a", params["input"][0]["url"]["blobId"].as_str().unwrap()).is_some());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn shrinking_keeps_exact_identities_and_never_corrupts_a_blob_reference() {
        let id = "i".repeat(12_000);
        let descriptor = json!({"type":"cutexBlob","blobId":"a".repeat(64),"sha256":"a".repeat(64),"mime":"image/png","size":42,"url":format!("cutex-blob:cutex.a/{}","a".repeat(64))});
        let store = EventBlobStore::at(std::env::temp_dir().join("unused-event-store"));
        let mut event = json!({"method":"item/completed","params":{"threadId":id,"turnId":"turn1","item":{"id":"item1","status":"completed","result":descriptor,"other":"x".repeat(1_000_000)}}});
        store.normalize("cutex.a", &mut event).unwrap();
        assert_eq!(event["params"]["threadId"], id);
        assert_eq!(event["params"]["item"]["result"], descriptor);
        assert!(encoded_fits(&event, PAYLOAD_BYTES));
        let mut impossible = json!({"method":"x".repeat(MAX_EVENT_BYTES*2)});
        assert!(store.normalize("cutex.a", &mut impossible).is_err());
    }

    #[test]
    fn malformed_media_is_explicit_and_mime_aliases_do_not_replace_grants() {
        let dir = std::env::temp_dir().join(format!("ebm-{}", uuid::Uuid::new_v4()));
        let store = EventBlobStore::at(&dir);
        let mut invalid = json!({"type":"imageGeneration","result":"!".repeat(65536)});
        store.normalize("cutex.a", &mut invalid).unwrap();
        assert_eq!(invalid["result"]["reason"], "invalid_base64");
        let first = store.put("cutex.a", "audio/wav", b"test bytes").unwrap();
        let alias = store.put("cutex.a", "audio/x-wav", b"test bytes").unwrap();
        assert_eq!(first, alias);
        assert_eq!(
            store
                .get("cutex.a", first["sha256"].as_str().unwrap())
                .unwrap()
                .0,
            "audio/wav"
        );
        assert!(store
            .get("cutex.b", first["sha256"].as_str().unwrap())
            .is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn decoded_limit_is_checked_even_when_base64_rounding_fits_the_text_budget() {
        let store = EventBlobStore::at(std::env::temp_dir().join("unused-oversize-event-store"));
        let mut event = json!({"type":"imageGeneration","result":base64::engine::general_purpose::STANDARD.encode(vec![0;MAX_BLOB_BYTES+1])});
        store.normalize("cutex.a", &mut event).unwrap();
        assert_eq!(event["result"]["reason"], "blob_size_limit");
    }

    #[test]
    fn rpc_media_externalization_preserves_full_history_text_without_event_truncation() {
        let dir = std::env::temp_dir().join(format!("ebh-{}", uuid::Uuid::new_v4()));
        let store = EventBlobStore::at(&dir);
        let mut response = json!({"text":"x".repeat(512*1024),"image":{"b64_json":base64::engine::general_purpose::STANDARD.encode(vec![0;65536])}});
        store.externalize_media("cutex.a", &mut response).unwrap();
        assert_eq!(response["text"].as_str().unwrap().len(), 512 * 1024);
        assert_eq!(response["image"]["b64_json"]["type"], "cutexBlob");
        assert!(response.get("_cutexTruncated").is_none());
        fs::remove_dir_all(dir).unwrap();
    }
}
