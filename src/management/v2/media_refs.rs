//! `?media=ref` on `GET /v2/sessions/{id}/bootstrap`, `/history` and
//! `/history/turns/{turnId}` (capability `history-media-refs`, cutex-desktop
//! docs/design/025-large-conversations.md).
//!
//! An image-generating agent's history held 163 generated PNGs as base64 in
//! `imageGeneration.result` plus the same images again as `data:` URLs in
//! tool outputs: 1.49 GB of rollout, ~1.3 GB for the desktop's default page
//! of 30 turns, more than a JavaScript string can hold (host-a 2026-10-22).
//! With `media=ref` every media payload over [`INLINE_MAX`] is stored once in
//! the media cache (content-addressed, [`CACHE_MAX_BYTES`], least recently
//! used first out) and replaced by `cutex-media:<cutexSessionId>/<sha256>`,
//! read back with `GET /v2/sessions/{id}/media/{sha256}`. Any other string
//! over [`TEXT_MAX`] inside a tool item is cut and the item marked
//! `truncated` (as `compact`). Without the parameter nothing changes.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::Context;
use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::http::server::SimpleHttpRequest;

#[path = "media_saved.rs"]
mod saved;
pub use saved::session_supports_saved_images;

/// Media payloads (base64 text) longer than this become references.
pub const INLINE_MAX: usize = 64 * 1024;
/// Other strings in tool items are cut to this many bytes.
pub const TEXT_MAX: usize = 1024 * 1024;
/// Media cache size; least recently used files go first.
pub const CACHE_MAX_BYTES: u64 = 2 << 30;
pub const REF_PREFIX: &str = "cutex-media:";

/// `?media=` value: absent / `inline` = off, `ref` = on.
pub fn parse_media(request: &SimpleHttpRequest) -> Result<bool, String> {
    match super::workspace::query_params(request).get("media").map(String::as_str) {
        None | Some("inline") => Ok(false),
        Some("ref") => Ok(true),
        Some(_) => Err("media must be ref or inline".to_string()),
    }
}

/// `/v2/sessions/{id}/media/{sha256}`.
pub fn parse_path(path: &str) -> Option<(String, String)> {
    let (session, sha) = path.strip_prefix("/v2/sessions/")?.split_once("/media/")?;
    let session = super::server::decode_path_segment(session)?;
    (sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())).then(|| (session, sha.to_string()))
}

pub struct MediaStore {
    dir: PathBuf,
    max_bytes: u64,
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Meta {
    mime: String,
    sessions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<saved::Source>,
}

impl MediaStore {
    pub fn open_default() -> anyhow::Result<Self> {
        Ok(Self::at(crate::config::paths::runtime_dir()?.join("management-v2").join("history-media"), CACHE_MAX_BYTES))
    }

    pub fn at(dir: impl Into<PathBuf>, max_bytes: u64) -> Self {
        Self { dir: dir.into(), max_bytes }
    }

    fn paths(&self, sha: &str) -> (PathBuf, PathBuf) {
        (self.dir.join(format!("{sha}.bin")), self.dir.join(format!("{sha}.json")))
    }

    /// A stable sidecar (never renamed or evicted), shared by metadata RMW, readers and eviction.
    /// Locking the replaceable JSON itself would leave writers locking different inodes after a rename.
    fn lock(&self) -> anyhow::Result<fs::File> {
        self.lock_mode(false)
    }

    fn lock_mode(&self, shared: bool) -> anyhow::Result<fs::File> {
        fs::create_dir_all(&self.dir).with_context(|| format!("create {}", self.dir.display()))?;
        let mut options = fs::OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(self.dir.join(".media.lock"))?;
        if shared {
            fs2::FileExt::lock_shared(&lock).context("read-lock history media cache")?;
        } else {
            fs2::FileExt::lock_exclusive(&lock).context("lock history media cache")?;
        }
        Ok(lock)
    }

    /// Store `bytes` under `sha` unless present (then only touched); records
    /// that `session` may read it.
    fn put(&self, sha: &str, mime: &str, session: &str, bytes: Option<Vec<u8>>) -> anyhow::Result<()> {
        let _lock = self.lock()?;
        let (bin, meta_path) = self.paths(sha);
        // Eviction may have removed the bin since the caller's cache lookup. Never publish a
        // session grant without bytes; the replacer can decode and store the payload again.
        let mut meta: Meta = saved::read_meta(&meta_path).unwrap_or_default();
        anyhow::ensure!(bytes.is_some() || bin.exists() || meta.source.is_some(), "history media was evicted");
        // Only inspect bytes already decoded for this write; cache hits do not
        // read or decode an image just to backfill layout hints.
        let dimensions = bytes.as_deref().and_then(super::image_dimensions::dimensions);
        match bytes {
            Some(bytes) if !bin.exists() => write_atomic(&bin, &bytes)?,
            // Touch: recently used media stay in the cache.
            _ => {
                let _ = filetime_now(if meta.source.is_some() { &meta_path } else { &bin });
            }
        }
        let mut changed = false;
        if let Some((width, height)) = dimensions {
            if meta.width != Some(width) || meta.height != Some(height) {
                meta.width = Some(width);
                meta.height = Some(height);
                changed = true;
            }
        }
        if meta.mime.is_empty() {
            meta.mime = mime.to_string();
            changed = true;
        }
        if !meta.sessions.iter().any(|known| known == session) {
            meta.sessions.push(session.to_string());
            changed = true;
        }
        if changed {
            let encoded = serde_json::to_vec(&meta)?;
            anyhow::ensure!(meta.source.is_none() || encoded.len() as u64 <= saved::MAX_META_BYTES, "saved image grants exceed metadata limit");
            write_atomic(&meta_path, &encoded)?;
        }
        Ok(())
    }

    /// What `Replacer` does for a payload whose bytes are already stored:
    /// touch them and record that `session` may read them. False (nothing
    /// done) when they are no longer in the cache.
    pub(super) fn reuse(&self, sha: &str, mime: &str, session: &str) -> bool {
        self.put(sha, mime, session, None).is_ok()
    }

    /// The bytes and type of `sha` when `session` may read them.
    pub fn get(&self, session: &str, sha: &str) -> Option<(String, Vec<u8>)> {
        self.get_with_saved_files(session, sha, false)
    }

    pub fn get_with_saved_files(&self, session: &str, sha: &str, allow_saved: bool) -> Option<(String, Vec<u8>)> {
        let _lock = self.lock_mode(true).ok()?;
        let (bin, meta_path) = self.paths(sha);
        let meta: Meta = saved::read_meta(&meta_path)?;
        if !meta.sessions.iter().any(|known| known == session) {
            return None;
        }
        if let Some(source) = &meta.source {
            return allow_saved.then(|| source.read().ok()).flatten();
        }
        let bytes = fs::read(&bin).ok()?;
        let _ = filetime_now(&bin);
        Some((meta.mime, bytes))
    }

    /// Drop the least recently used media until the cache fits.
    pub fn evict(&self) {
        let Ok(_lock) = self.lock() else { return };
        saved::evict_descriptors(&self.dir);
        let Ok(entries) = fs::read_dir(&self.dir) else { return };
        let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = entries
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "bin"))
            .filter_map(|entry| {
                let meta = entry.metadata().ok()?;
                Some((meta.modified().ok()?, meta.len(), entry.path()))
            })
            .collect();
        let mut total: u64 = files.iter().map(|(_, len, _)| len).sum();
        if total <= self.max_bytes {
            return;
        }
        files.sort();
        for (_, len, path) in files {
            if total <= self.max_bytes {
                break;
            }
            let _ = fs::remove_file(&path);
            let _ = fs::remove_file(path.with_extension("json"));
            total = total.saturating_sub(len);
        }
    }
}

fn filetime_now(path: &std::path::Path) -> std::io::Result<()> {
    fs::File::options().write(true).open(path)?.set_modified(std::time::SystemTime::now())
}

fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> anyhow::Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    let mut file = fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    file.write_all(bytes)?;
    drop(file);
    fs::rename(&tmp, path).with_context(|| format!("rename {}", path.display()))?;
    Ok(())
}

fn sha_hex(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// The type of decoded image bytes by their signature.
fn sniff(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        "application/octet-stream"
    }
}

fn decode(base64: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(base64.trim()).ok()
}

/// What replaces media in a response.
pub struct Replacer<'a> {
    pub store: &'a MediaStore,
    pub session: &'a str,
    pub replaced: usize,
    /// Every stored reference as `(sha256, mime passed to the store)`, in
    /// order, so a cached shaped turn can repeat the store's side effects
    /// (`history_turns`).
    pub recorded: Vec<(String, String)>,
    /// Payloads left inline because they could not be decoded or stored.
    pub failed: usize,
    pub(super) saved_files: bool,
}

impl<'a> Replacer<'a> {
    pub fn new(store: &'a MediaStore, session: &'a str) -> Self {
        Self { store, session, replaced: 0, recorded: Vec::new(), failed: 0, saved_files: false }
    }
}

impl Replacer<'_> {
    pub(super) fn with_saved_files(mut self, enabled: bool) -> Self {
        self.saved_files = enabled;
        self
    }

    /// Store `payload` (base64, `mime` or sniffed) and return its reference;
    /// None leaves it inline (undecodable, or the cache failed).
    fn reference(&mut self, payload: &str, mime: Option<&str>) -> Option<String> {
        let sha = sha_hex(payload);
        let hint = mime.unwrap_or("");
        let mime = if self.store.reuse(&sha, hint, self.session) {
            hint.to_string()
        } else {
            let Some(bytes) = decode(payload) else {
                self.failed += 1;
                return None;
            };
            let mime = mime.map_or_else(|| sniff(&bytes).to_string(), str::to_string);
            if let Err(error) = self.store.put(&sha, &mime, self.session, Some(bytes)) {
                eprintln!("history media: could not store {sha}: {error:#}");
                self.failed += 1;
                return None;
            }
            mime
        };
        self.replaced += 1;
        let reference = format!("{REF_PREFIX}{}/{sha}", self.session);
        self.recorded.push((sha, mime));
        Some(reference)
    }

    fn data_url(&mut self, text: &mut String) {
        let Some(rest) = text.strip_prefix("data:") else { return };
        let Some((mime, payload)) = rest.split_once(";base64,") else { return };
        if text.len() <= INLINE_MAX || mime.contains(',') {
            return;
        }
        let mime = mime.to_string();
        if let Some(reference) = self.reference(payload, Some(&mime)) {
            *text = reference;
        }
    }

    /// Replace every media payload over [`INLINE_MAX`] in `value`.
    pub fn replace(&mut self, value: &mut Value) {
        match value {
            Value::Object(map) => {
                let kind = map.get("type").and_then(Value::as_str).map(str::to_string);
                // Generated image: raw base64 in `result`.
                if kind.as_deref() == Some("imageGeneration") {
                    if let Some(Value::String(result)) = map.get_mut("result") {
                        if result.len() > INLINE_MAX && !result.starts_with("data:") {
                            if let Some(reference) = self.reference(result, None) {
                                *result = reference;
                            }
                        }
                    }
                }
                // MCP image / audio block: raw base64 in `data` with `mimeType`.
                if matches!(kind.as_deref(), Some("image" | "audio")) {
                    let mime = map.get("mimeType").and_then(Value::as_str).map(str::to_string);
                    if let Some(Value::String(data)) = map.get_mut("data") {
                        if data.len() > INLINE_MAX && !data.starts_with("data:") {
                            if let Some(reference) = self.reference(data, mime.as_deref()) {
                                *data = reference;
                            }
                        }
                    }
                }
                for item in map.values_mut() {
                    self.replace(item);
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|item| self.replace(item)),
            Value::String(text) if text.len() > INLINE_MAX => self.data_url(text),
            _ => {}
        }
    }
}

/// Apply `media=ref` to a history / bootstrap response body: media become
/// references, other huge tool strings are cut; adds `mediaRefs`.
pub fn apply(body: &mut Value, session: &str, store: &MediaStore) -> usize {
    apply_with_saved_files(body, session, store, false)
}

pub(super) fn apply_with_saved_files(body: &mut Value, session: &str, store: &MediaStore, enabled: bool) -> usize {
    let mut replacer = Replacer::new(store, session).with_saved_files(enabled);
    replacer.replace_saved_body(body);
    replacer.replace(body);
    let cut = super::compact::cap_tool_strings(body, TEXT_MAX);
    store.evict();
    if let Some(map) = body.as_object_mut() {
        map.insert("mediaRefs".to_string(), Value::from(replacer.replaced));
    }
    replacer.replaced + cut
}

/// `apply` on the default media cache; a cache that cannot be opened leaves
/// the media inline (logged).
pub fn apply_default(body: &mut Value, session: &str) {
    apply_default_with_saved_files(body, session, false);
}

pub(super) fn apply_default_with_saved_files(body: &mut Value, session: &str, enabled: bool) {
    match MediaStore::open_default() {
        Ok(store) => {
            apply_with_saved_files(body, session, &store, enabled);
        }
        Err(error) => eprintln!("history media: cache unavailable: {error:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_to_legacy_ref_records_dimensions_only_when_bytes_are_available() {
        let dir = std::env::temp_dir().join(format!("mrd-{}", uuid::Uuid::new_v4()));
        let store = MediaStore::at(&dir, CACHE_MAX_BYTES);
        let mut bytes = base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aWQAAAABJRU5ErkJggg==").unwrap();
        bytes.resize(INLINE_MAX, 0);
        let payload = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let sha = sha_hex(&payload);
        let mut item = serde_json::json!({"type":"imageGeneration","result":payload});
        Replacer::new(&store, "cutex.a").replace(&mut item);
        assert_eq!(item["result"], format!("{REF_PREFIX}cutex.a/{sha}"));
        let meta: Meta = saved::read_meta(&store.paths(&sha).1).unwrap();
        assert_eq!((meta.width, meta.height), (Some(1), Some(1)));
        assert_eq!(store.get("cutex.a", &sha).unwrap().1, bytes);
        // Old metadata remains readable and a reference cache hit need not
        // load its image to fill optional layout fields.
        write_atomic(&store.paths(&sha).1, br#"{"mime":"image/png","sessions":["cutex.a"]}"#).unwrap();
        assert!(store.reuse(&sha, "image/png", "cutex.a"));
        let meta: Meta = saved::read_meta(&store.paths(&sha).1).unwrap();
        assert_eq!((meta.width, meta.height), (None, None));
        fs::remove_dir_all(dir).unwrap();
    }
    use serde_json::json;
    use std::collections::HashMap;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest";

    #[test]
    fn concurrent_reads_share_the_guard_and_keep_session_access_checks() {
        let (store,dir)=temp_store("shared-read",CACHE_MAX_BYTES);
        store.put("shared","image/png","one",Some(PNG.to_vec())).unwrap();
        let guard=store.lock_mode(true).unwrap();
        let other=MediaStore::at(&dir,CACHE_MAX_BYTES);
        let (tx,rx)=std::sync::mpsc::channel();
        let worker=std::thread::spawn(move||{tx.send((other.get("one","shared"),other.get("denied","shared"))).unwrap();});
        let result=rx.recv_timeout(std::time::Duration::from_secs(5));
        drop(guard);worker.join().unwrap();
        let (allowed,denied)=result.expect("a read must not take the exclusive writer guard");
        assert_eq!(allowed.unwrap().1,PNG);assert!(denied.is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    fn big_png_base64(fill: u8) -> String {
        let mut bytes = PNG.to_vec();
        bytes.extend(std::iter::repeat_n(fill, INLINE_MAX));
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn temp_store(name: &str, max: u64) -> (MediaStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!("cutex-media-{name}-{}", uuid::Uuid::new_v4()));
        (MediaStore::at(&dir, max), dir)
    }

    #[test]
    fn media_store_process_writer() {
        let Some(dir) = std::env::var_os("CUTEX_MEDIA_LOCK_TEST_DIR") else { return };
        let session = std::env::var("CUTEX_MEDIA_LOCK_TEST_SESSION").unwrap();
        let store = MediaStore::at(PathBuf::from(dir), CACHE_MAX_BYTES);
        fs::write(store.dir.join(format!("ready-{session}")), b"").unwrap();
        for i in 0..8 {
            store.put("shared", "image/png", &format!("{session}-{i}"), Some(PNG.to_vec())).unwrap();
        }
    }

    #[test]
    fn concurrent_processes_preserve_all_media_session_grants() {
        let (store, dir) = temp_store("process-lock", CACHE_MAX_BYTES);
        let lock = store.lock().unwrap();
        let mut children: Vec<_> = (0..3).map(|i| {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "management::v2::media_refs::tests::media_store_process_writer", "--nocapture"])
                .env("CUTEX_MEDIA_LOCK_TEST_DIR", &dir)
                .env("CUTEX_MEDIA_LOCK_TEST_SESSION", format!("s{i}"))
                .spawn().unwrap()
        }).collect();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !(0..3).all(|i| dir.join(format!("ready-s{i}")).exists()) {
            assert!(std::time::Instant::now() < deadline, "media writer children did not reach the lock");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        drop(lock);
        for child in &mut children { assert!(child.wait().unwrap().success()); }
        for i in 0..3 { for j in 0..8 {
            assert_eq!(store.get(&format!("s{i}-{j}"), "shared").unwrap().1, PNG);
        } }
        let meta: Meta = serde_json::from_slice(&fs::read(store.paths("shared").1).unwrap()).unwrap();
        assert_eq!(meta.sessions.len(), 24);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn eviction_uses_the_metadata_lock_and_reuse_cannot_resurrect_a_missing_bin() {
        let (store, dir) = temp_store("eviction-lock", 0);
        store.put("shared", "image/png", "one", Some(PNG.to_vec())).unwrap();
        let lock = store.lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let other = MediaStore::at(&dir, 0);
        let worker = std::thread::spawn(move || { other.evict(); tx.send(()).unwrap(); });
        assert!(rx.recv_timeout(std::time::Duration::from_millis(100)).is_err(), "eviction ran during metadata RMW");
        assert!(store.paths("shared").0.exists());
        drop(lock);
        rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        worker.join().unwrap();
        assert!(!store.reuse("shared", "image/png", "two"));
        assert!(!store.paths("shared").0.exists());
        assert!(!store.paths("shared").1.exists(), "reuse must not recreate a grant without bytes");
        fs::remove_dir_all(dir).unwrap();
    }

    fn request(path: &str) -> SimpleHttpRequest {
        SimpleHttpRequest { method: "GET".into(), path: path.into(), headers: HashMap::new(), body: Vec::new() }
    }

    #[test]
    fn media_parameter_and_path() {
        assert_eq!(parse_media(&request("/v2/sessions/s/history")), Ok(false));
        assert_eq!(parse_media(&request("/v2/sessions/s/history?media=ref")), Ok(true));
        assert_eq!(parse_media(&request("/v2/sessions/s/history?media=inline")), Ok(false));
        assert!(parse_media(&request("/v2/sessions/s/history?media=yes")).is_err());
        // The history routes accept it next to their own parameters.
        assert!(super::super::history::parse_query(&request("/v2/sessions/s/history?limit=5&media=ref")).is_ok());
        assert!(super::super::history::parse_turn_query(&request("/v2/sessions/s/history/turns/t?media=ref&compact=1")).is_ok());
        assert!(super::super::history::parse_turn_query(&request("/v2/sessions/s/history/turns/t?other=1")).is_err());
        let sha = "a".repeat(64);
        assert_eq!(parse_path(&format!("/v2/sessions/cutex.s1/media/{sha}")), Some(("cutex.s1".into(), sha.clone())));
        assert_eq!(parse_path(&format!("/v2/sessions/cutex.s1/media/{}", "A".repeat(64))), None);
        assert_eq!(parse_path("/v2/sessions/cutex.s1/media/abc"), None);
        assert_eq!(parse_path(&format!("/v2/sessions/a/b/media/{sha}")), None);
    }

    /// host-a 2026-10-22: a synthetic image-generating thread — generated
    /// images in `imageGeneration.result`, the same images as `data:` URLs in
    /// tool output, MCP image blocks, a user's pasted image and a huge
    /// command output — shrinks to references and capped text; small media
    /// stay inline; the bytes read back through the cache, deduplicated.
    #[test]
    fn a_huge_image_thread_becomes_small_and_reads_back() {
        let (store, dir) = temp_store("thread", CACHE_MAX_BYTES);
        let mut turns = Vec::new();
        for n in 0..40u8 {
            let image = big_png_base64(n);
            turns.push(json!({"id": format!("turn-{n}"), "items": [
                {"type": "userMessage", "id": format!("u{n}"), "content": [
                    {"type": "text", "text": "draw"},
                    {"type": "image", "url": format!("data:image/png;base64,{image}")},
                    {"type": "image", "url": "data:image/png;base64,c21hbGw="}
                ]},
                {"type": "imageGeneration", "id": format!("ig{n}"), "status": "completed", "revisedPrompt": "p", "result": image.clone(), "savedPath": "C:/x.png"},
                {"type": "functionCallOutput", "id": format!("fco{n}"), "name": "js", "output": [
                    {"type": "inputText", "text": "Script completed"},
                    {"type": "inputImage", "imageUrl": format!("data:image/png;base64,{image}")}
                ]},
                {"type": "mcpToolCall", "id": format!("m{n}"), "server": "s", "tool": "t", "result": {"content": [
                    {"type": "image", "mimeType": "image/png", "data": image.clone()}
                ]}},
                {"type": "commandExecution", "id": format!("c{n}"), "command": "dump", "aggregatedOutput": "x".repeat(TEXT_MAX + 10), "status": "completed"},
                {"type": "agentMessage", "id": format!("a{n}"), "text": "y".repeat(TEXT_MAX + 10)}
            ]}));
        }
        let mut body = json!({"native": {"result": {"thread": {"id": "t", "turns": turns}}}});
        let before = body.to_string().len();
        apply(&mut body, "cutex.s1", &store);
        let after = body.to_string().len();
        assert!(before > 40 * 4 * INLINE_MAX, "synthetic thread is big: {before}");
        // Only the 40 agent messages (kept whole) and the capped outputs remain.
        assert!(after < 40 * (2 * TEXT_MAX + 64 * 1024), "{after}");
        assert_eq!(body["mediaRefs"], json!(40 * 4));
        let items = &body["native"]["result"]["thread"]["turns"][3]["items"];
        let reference = items[1]["result"].as_str().unwrap();
        assert!(reference.starts_with("cutex-media:cutex.s1/"), "{reference}");
        assert_eq!(items[0]["content"][1]["url"], json!(reference), "same image, same reference");
        assert_eq!(items[2]["output"][1]["imageUrl"], json!(reference));
        assert_eq!(items[3]["result"]["content"][0]["data"], json!(reference));
        assert_eq!(items[0]["content"][2]["url"], json!("data:image/png;base64,c21hbGw="), "small images stay inline");
        assert_eq!(items[4]["aggregatedOutput"].as_str().unwrap().len(), TEXT_MAX);
        assert_eq!(items[4]["truncated"], json!(true));
        assert_eq!(items[5]["text"].as_str().unwrap().len(), TEXT_MAX + 10, "agent text is not a tool payload");
        let sha = reference.rsplit('/').next().unwrap();
        let (mime, bytes) = store.get("cutex.s1", sha).expect("stored");
        assert_eq!(mime, "image/png");
        assert!(bytes.starts_with(PNG));
        assert!(store.get("cutex.other", sha).is_none(), "another session cannot read it");
        // 40 distinct images, each stored once.
        let bins = fs::read_dir(&dir).unwrap().filter(|e| e.as_ref().unwrap().path().extension().is_some_and(|x| x == "bin")).count();
        assert_eq!(bins, 40);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn the_cache_keeps_the_most_recently_used() {
        let (store, dir) = temp_store("evict", (3 * (PNG.len() + INLINE_MAX)) as u64);
        let mut refs = Vec::new();
        for n in 0..5u8 {
            let mut body = json!({"result": {"type": "imageGeneration", "id": "i", "result": big_png_base64(n)}});
            apply(&mut body, "cutex.s1", &store);
            refs.push(body["result"]["result"].as_str().unwrap().rsplit('/').next().unwrap().to_string());
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(store.get("cutex.s1", &refs[0]).is_none(), "oldest evicted");
        assert!(store.get("cutex.s1", &refs[4]).is_some());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn without_the_parameter_nothing_is_touched_and_bad_base64_stays_inline() {
        let (store, dir) = temp_store("bad", CACHE_MAX_BYTES);
        let mut body = json!({"type": "imageGeneration", "id": "i", "result": "!".repeat(INLINE_MAX + 1)});
        let original = body.clone();
        apply(&mut body, "cutex.s1", &store);
        assert_eq!(body["result"], original["result"], "undecodable payload is left as it was");
        assert_eq!(body["mediaRefs"], json!(0));
        let _ = fs::remove_dir_all(dir);
    }
}
