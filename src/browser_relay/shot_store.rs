//! B10 owner-only images. No page text, title, URL, credential or model result
//! is persisted. Optional index is independently bounded and atomically replaced.
use super::{
    images::{self, ValidImage},
    model::*,
    private_store::Directory,
};
use base64::Engine;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    path::Path,
    sync::{Mutex, OnceLock},
};
const MAX_SHOTS: usize = 1000;
const MAX_INDEX: usize = 1024 * 1024;
const MAX_BYTES: usize = 64 * 1024 * 1024 - MAX_INDEX;
const RETAIN_SECONDS: i64 = 7 * 24 * 60 * 60;
static LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Shot {
    pub id: String,
    pub turn_id: String,
    pub created_at: DateTime<Utc>,
    pub width: u32,
    pub height: u32,
    pub mime_type: String,
    session_id: String,
    thread_id: String,
    runtime_generation: u64,
    bytes: usize,
}
impl Shot {
    fn filename(&self) -> String {
        format!(
            "{}.{}",
            self.id,
            if self.mime_type == "image/png" {
                "png"
            } else {
                "jpg"
            }
        )
    }
    fn view(&self) -> Value {
        json!({"id":self.id,"turnId":self.turn_id,"createdAt":self.created_at.to_rfc3339(),"width":self.width,"height":self.height,"mimeType":self.mime_type})
    }
    fn valid(&self) -> bool {
        shot_id(&self.id)
            && identifier(&self.session_id)
            && identifier(&self.thread_id)
            && identifier(&self.turn_id)
            && self.width > 0
            && self.height > 0
            && self.width <= 1600
            && self.height <= 1600
            && matches!(self.mime_type.as_str(), "image/png" | "image/jpeg")
            && self.bytes > 0
            && self.bytes <= images::MAX_COMPRESSED
    }
}
fn shot_id(id: &str) -> bool {
    id.strip_prefix("shot-").is_some_and(|v| {
        v.len() == 32
            && v.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    })
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    version: u8,
    items: Vec<Shot>,
}
struct Store {
    root: Directory,
    index: Index,
}
impl Store {
    fn open(path: &Path, now: DateTime<Utc>) -> anyhow::Result<Self> {
        let root = Directory::open(path)?;
        let index = match root.read("index.json", MAX_INDEX)? {
            Some(bytes) => serde_json::from_slice(&bytes)?,
            None => Index {
                version: 1,
                items: Vec::new(),
            },
        };
        let mut store = Self { root, index };
        let mut ids = HashSet::new();
        anyhow::ensure!(
            store.index.version == 1
                && store.index.items.len() <= MAX_SHOTS
                && store
                    .index
                    .items
                    .iter()
                    .all(|s| s.valid() && ids.insert(s.id.clone())),
            "invalid browser shot index"
        );
        store.prune(now, 0, 0)?;
        // Only our fixed random-name format is eligible for orphan cleanup.
        // Unknown files or too many directory entries fail/are left alone.
        let retained: HashSet<_> = store.index.items.iter().map(Shot::filename).collect();
        for name in store.root.names(2 * MAX_SHOTS + 16)? {
            let orphan = name
                .rsplit_once('.')
                .is_some_and(|(id, ext)| shot_id(id) && matches!(ext, "png" | "jpg"))
                && !retained.contains(&name);
            let temp = name
                .strip_prefix("tmp-")
                .and_then(|s| s.strip_suffix(".json"))
                .is_some_and(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()));
            if orphan || temp {
                store.root.remove(&name)?;
            }
        }
        Ok(store)
    }
    fn save(&self) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec(&self.index)?;
        anyhow::ensure!(bytes.len() <= MAX_INDEX, "browser shot index too large");
        self.root.atomic("index.json", &bytes)
    }
    fn prune(
        &mut self,
        now: DateTime<Utc>,
        extra_bytes: usize,
        extra_count: usize,
    ) -> anyhow::Result<()> {
        self.index.items.sort_by_key(|s| s.created_at);
        let mut removed = Vec::new();
        let mut total: usize = self.index.items.iter().map(|s| s.bytes).sum();
        while self.index.items.first().is_some_and(|s| {
            now.signed_duration_since(s.created_at).num_seconds() >= RETAIN_SECONDS
                || s.created_at > now + chrono::Duration::minutes(5)
        }) || total + extra_bytes > MAX_BYTES
            || self.index.items.len() + extra_count > MAX_SHOTS
        {
            anyhow::ensure!(
                !self.index.items.is_empty(),
                "browser shot exceeds store budget"
            );
            let shot = self.index.items.remove(0);
            total = total.saturating_sub(shot.bytes);
            removed.push(shot.filename());
        }
        // Commit loss of reachability first. A crash leaves an orphan for the
        // next bounded sweep; it never points an index at a replacement file.
        if !removed.is_empty() {
            self.save()?;
            for name in removed {
                self.root.remove(&name)?;
            }
        }
        Ok(())
    }
    fn insert_checked(
        &mut self,
        agent: &super::auth::AgentContext,
        image: &ValidImage,
        now: DateTime<Utc>,
        valid: impl Fn() -> bool,
    ) -> anyhow::Result<Value> {
        anyhow::ensure!(
            identifier(&agent.session) && identifier(&agent.thread) && identifier(&agent.turn),
            "invalid browser shot binding"
        );
        self.prune(now, image.bytes.len(), 1)?;
        let shot = Shot {
            id: format!("shot-{}", uuid::Uuid::new_v4().simple()),
            turn_id: agent.turn.clone(),
            created_at: now,
            width: image.width,
            height: image.height,
            mime_type: image.mime.into(),
            session_id: agent.session.clone(),
            thread_id: agent.thread.clone(),
            runtime_generation: agent.generation,
            bytes: image.bytes.len(),
        };
        self.root.write_new(&shot.filename(), &image.bytes)?;
        if !valid() {
            self.root.remove(&shot.filename())?;
            anyhow::bail!("browser capture binding changed");
        }
        let view = shot.view();
        self.index.items.push(shot);
        if let Err(error) = self.save() {
            let shot = self.index.items.pop().unwrap();
            let _ = self.root.remove(&shot.filename());
            return Err(error);
        }
        Ok(view)
    }
    fn list(&self, session: &str, turn: &str) -> Value {
        json!({"items":self.index.items.iter().rev().filter(|s|s.session_id==session&&s.turn_id==turn).take(256).map(Shot::view).collect::<Vec<_>>()})
    }
    fn read(&self, id: &str) -> anyhow::Result<Value> {
        anyhow::ensure!(shot_id(id), "invalid browser shot id");
        let shot = self
            .index
            .items
            .iter()
            .find(|s| s.id == id)
            .ok_or_else(|| anyhow::anyhow!("browser shot not retained"))?;
        let bytes = self
            .root
            .read(&shot.filename(), images::MAX_COMPRESSED)?
            .ok_or_else(|| anyhow::anyhow!("browser shot not retained"))?;
        anyhow::ensure!(bytes.len() == shot.bytes, "browser shot size changed");
        let image = images::decode_bytes_owner(bytes, &shot.mime_type)?;
        anyhow::ensure!(
            image.width == shot.width && image.height == shot.height,
            "browser shot dimensions changed"
        );
        Ok(
            json!({"mimeType":image.mime,"data":base64::engine::general_purpose::STANDARD.encode(image.bytes)}),
        )
    }
}
fn apply<T>(f: impl FnOnce(&mut Store) -> anyhow::Result<T>) -> Result<T> {
    let _lock = LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| RelayError::new("browser_unavailable"))?;
    let path = crate::config::paths::runtime_dir()
        .map_err(|_| RelayError::new("browser_unavailable"))?
        .join("browser")
        .join("turn-shots");
    let mut store =
        Store::open(&path, Utc::now()).map_err(|_| RelayError::new("browser_unavailable"))?;
    f(&mut store).map_err(store_error)
}
fn store_error(error: anyhow::Error) -> RelayError {
    // Resource contention is transient, not evidence that the retained image
    // was lost. Never expose arbitrary filesystem/decoder exception text.
    if error
        .downcast_ref::<RelayError>()
        .is_some_and(|error| error.code == "busy")
    {
        RelayError::new("busy")
    } else {
        RelayError::new("request_result_not_retained")
    }
}
pub fn insert(
    agent: &super::auth::AgentContext,
    image: &ValidImage,
    valid: impl Fn() -> bool,
) -> Result<Value> {
    apply(|store| {
        let view = store.insert_checked(agent, image, Utc::now(), &valid)?;
        if !valid() {
            let id = view["id"].as_str().unwrap();
            if let Some(index) = store.index.items.iter().position(|s| s.id == id) {
                let shot = store.index.items.remove(index);
                store.save()?;
                store.root.remove(&shot.filename())?;
            }
            anyhow::bail!("browser capture binding changed");
        }
        Ok(view)
    })
}
pub fn list(session: &str, turn: &str) -> Result<Value> {
    if !identifier(session) || !identifier(turn) {
        return Err(RelayError::new("invalid_request"));
    }
    apply(|store| Ok(store.list(session, turn)))
}
pub fn read(id: &str) -> Result<Value> {
    if !shot_id(id) {
        return Err(RelayError::new("invalid_request"));
    }
    apply(|store| store.read(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn image() -> ValidImage {
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 3)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        images::decode_bytes(out.into_inner(), "image/png").unwrap()
    }
    fn agent() -> super::super::auth::AgentContext {
        super::super::auth::AgentContext {
            session: "cutex.s".into(),
            thread: "t".into(),
            generation: 1,
            turn: "turn".into(),
            runtime: "runtime".into(),
            credential_identity: "secret-not-stored".into(),
        }
    }
    #[test]
    fn browser_shot_store_roundtrip_scope_expiry_and_no_secret() {
        let _decode_guard = super::super::images::TEST_DECODE_LOCK.lock().unwrap();
        let temp = std::env::temp_dir().join(format!("browser-shots-{}", uuid::Uuid::new_v4()));
        let now = Utc::now();
        let mut store = Store::open(&temp, now).unwrap();
        assert!(store
            .insert_checked(&agent(), &image(), now, || false)
            .is_err());
        assert_eq!(store.list("cutex.s", "turn"), json!({"items":[]}));
        let view = store
            .insert_checked(&agent(), &image(), now, || true)
            .unwrap();
        assert_eq!(store.list("other", "turn"), json!({"items":[]}));
        assert_eq!(store.list("cutex.s", "turn")["items"][0], view);
        assert_eq!(
            store.read(view["id"].as_str().unwrap()).unwrap()["mimeType"],
            "image/png"
        );
        let index = store.root.read("index.json", MAX_INDEX).unwrap().unwrap();
        assert!(!String::from_utf8(index).unwrap().contains("secret"));
        assert!(store.read("../index.json").is_err());
        drop(store);
        let store = Store::open(&temp, now + chrono::Duration::days(8)).unwrap();
        assert_eq!(store.list("cutex.s", "turn"), json!({"items":[]}));
        assert!(store.read(view["id"].as_str().unwrap()).is_err());
        store.root.remove("index.json").unwrap();
        drop(store);
        std::fs::remove_dir(temp).unwrap();
    }
    #[test]
    fn browser_shot_store_bad_index_fails_closed() {
        let temp =
            std::env::temp_dir().join(format!("browser-shots-invalid-{}", uuid::Uuid::new_v4()));
        let root = Directory::open(&temp).unwrap();
        root.write_new("index.json", br#"{"version":1,"items":[],"path":"secret"}"#)
            .unwrap();
        assert!(Store::open(&temp, Utc::now()).is_err());
        root.remove("index.json").unwrap();
        drop(root);
        std::fs::remove_dir(temp).unwrap();
    }
    #[test]
    fn browser_shot_store_busy_is_transient_and_keeps_the_retained_image() {
        let _decode_guard = super::super::images::TEST_DECODE_LOCK.lock().unwrap();
        let temp =
            std::env::temp_dir().join(format!("browser-shots-busy-{}", uuid::Uuid::new_v4()));
        let now = Utc::now();
        let image = image();
        let mut store = Store::open(&temp, now).unwrap();
        let slot1 = images::DecodeSlot::acquire().unwrap();
        let slot2 = images::DecodeSlot::acquire().unwrap();
        // B10 inserts the already-validated image without a second decode,
        // even while both decoder slots are occupied by other requests.
        let view = store
            .insert_checked(&agent(), &image, now, || true)
            .unwrap();
        let id = view["id"].as_str().unwrap();
        assert_eq!(store_error(store.read(id).unwrap_err()).code, "busy");
        assert_eq!(store.list("cutex.s", "turn")["items"][0], view);
        assert!(store
            .root
            .read(&store.index.items[0].filename(), images::MAX_COMPRESSED)
            .unwrap()
            .is_some());
        drop(slot1);
        drop(slot2);
        assert_eq!(store.read(id).unwrap()["mimeType"], "image/png");
        store.prune(now + chrono::Duration::days(8), 0, 0).unwrap();
        store.root.remove("index.json").unwrap();
        drop(store);
        std::fs::remove_dir(temp).unwrap();
        assert_eq!(
            store_error(anyhow::anyhow!("secret decoder text")).code,
            "request_result_not_retained"
        );
    }
}
