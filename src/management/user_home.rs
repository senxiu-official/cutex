//! Per-user home page settings on the host (capability `user-home`;
//! cutex-desktop docs/research/home-page-20261019.md §1): one JSON document
//! per user, so every device of that person shows the same home page.
//!
//! - Store: `<cutex home>/runtime/users/home/<userId>.json` (schema
//!   `cutex/user-home/v1`, `{schema, revision, home, cover?, updatedAt?}`,
//!   private atomic writes under `home/home.lock`) and the cover image in
//!   `runtime/users/home/<userId>-cover-<etag>.<ext>`. The owner (`owner`)
//!   and every user have their own file; user ids are fixed-shape
//!   ([`crate::management::users::valid_user_id`]), so they are safe file
//!   names. Kept out of `users-v1.json` / `profiles-v1.json`: an older cutex
//!   never reads or rewrites these files.
//! - `revision` starts at 0 (no document) and grows by one on every change of
//!   `home`; a write names the revision it was based on (optimistic
//!   concurrency, 409 `home_revision_conflict` with the current revision).
//!   Cover changes do not change `revision` (they never conflict with an
//!   edit of the settings).
//! - `home` is client-owned and forward compatible: unknown top-level keys
//!   are kept; the known ones are checked ([`validate_home`]); the whole
//!   document is at most [`MAX_HOME_BYTES`] (compact JSON). `coverEtag` is
//!   server-owned (dropped from writes, added to reads).
//! - Cover: PNG / JPEG / WebP, at most [`MAX_COVER_BYTES`], any aspect, each
//!   side 1..=[`MAX_COVER_SIDE`] px (same checks as user avatars).

use std::fs::OpenOptions;
use std::path::PathBuf;

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::management::user_profiles::{validate_image, Avatar, AvatarUpload, ImageRules};
use crate::management::users::{audit, valid_user_id, UserError};

pub const SCHEMA: &str = "cutex/user-home/v1";
pub const MAX_HOME_BYTES: usize = 64 * 1024;
pub const MAX_COVER_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_COVER_SIDE: u32 = 4096;
pub const MAX_BIO_CHARS: usize = 200;
pub const MAX_PINS: usize = 64;
pub const MAX_BLOCKS: usize = 64;
/// `page` (capability `user-home-page-pins`): a client page, `key` =
/// `<pageId>:<argument>` ([`valid_page_key`]), e.g. `project-view:<projectId>`.
pub const PIN_KINDS: &[&str] = &["service", "artifact", "agent", "url", "page"];
const MAX_PAGE_ID_CHARS: usize = 64;
pub const PIN_SIZES: &[&str] = &["s", "m", "w", "l"];
const MAX_SHORT_CHARS: usize = 64;
const MAX_ID_CHARS: usize = 256;
const MAX_KEY_CHARS: usize = 2048;
const MAX_TITLE_CHARS: usize = 200;

pub const COVER_RULES: ImageRules =
    ImageRules { max_bytes: MAX_COVER_BYTES, min_side: 1, max_side: MAX_COVER_SIDE, square: false };

/// One user's stored home page.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    pub schema: String,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub home: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover: Option<Avatar>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

impl Default for Record {
    fn default() -> Self {
        Self { schema: SCHEMA.into(), revision: 0, home: None, cover: None, updated_at: None }
    }
}

impl Record {
    pub fn cover_etag(&self) -> Option<&str> {
        self.cover.as_ref().map(|cover| cover.etag.as_str())
    }

    /// `GET /v2/users/me/home`: `{revision, home: {..., coverEtag}|null,
    /// coverEtag, updatedAt}`.
    pub fn view(&self) -> Value {
        let cover = self.cover_etag();
        let home = self.home.clone().map(|mut home| {
            home.insert("coverEtag".into(), json!(cover));
            Value::Object(home)
        });
        json!({
            "revision": self.revision,
            "home": home,
            "coverEtag": cover,
            "updatedAt": self.updated_at,
        })
    }
}

fn invalid(message: impl Into<String>) -> UserError {
    UserError::new(400, "invalid_home", message)
}

fn short_text(value: &Value, field: &str, max: usize, nullable: bool) -> Result<(), UserError> {
    match value {
        Value::Null if nullable => Ok(()),
        Value::String(text) if !text.is_empty() && text.chars().count() <= max && !text.chars().any(char::is_control) => Ok(()),
        _ => Err(invalid(format!(
            "{field} must be a string of 1..={max} characters without control characters{}",
            if nullable { " or null" } else { "" }
        ))),
    }
}

/// A `page` pin's key: `<pageId>:<argument>`; the page id is 1..=64 of
/// `[a-z0-9._-]` starting with a letter, the argument is not empty (what it
/// means is the client's).
pub fn valid_page_key(key: &str) -> bool {
    let Some((id, argument)) = key.split_once(':') else { return false };
    !argument.is_empty()
        && id.len() <= MAX_PAGE_ID_CHARS
        && id.starts_with(|c: char| c.is_ascii_lowercase())
        && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

fn boolean(value: &Value, field: &str) -> Result<(), UserError> {
    match value {
        Value::Null | Value::Bool(_) => Ok(()),
        _ => Err(invalid(format!("{field} must be a boolean or null"))),
    }
}

/// Check a `home` document; `null` clears it. Returns what to store (the
/// object without the server-owned `coverEtag`).
pub fn validate_home(value: &Value) -> Result<Option<Map<String, Value>>, UserError> {
    let mut home = match value {
        Value::Null => return Ok(None),
        Value::Object(home) => home.clone(),
        _ => return Err(invalid("home must be an object or null")),
    };
    home.remove("coverEtag");
    let size = serde_json::to_vec(&home).map(|bytes| bytes.len()).unwrap_or(usize::MAX);
    if size > MAX_HOME_BYTES {
        let mut error = UserError::new(413, "home_too_large", format!("home may be at most {MAX_HOME_BYTES} bytes of JSON"));
        error.details = json!({ "bytes": size, "limit": MAX_HOME_BYTES });
        return Err(error);
    }
    if let Some(layout) = home.get("layout") {
        short_text(layout, "layout", MAX_SHORT_CHARS, true)?;
    }
    if let Some(bio) = home.get("bio") {
        match bio {
            Value::Null => {}
            Value::String(text)
                if text.chars().count() <= MAX_BIO_CHARS
                    && !text.chars().any(|c| c.is_control() && c != '\n') => {}
            _ => return Err(invalid(format!("bio must be a string of at most {MAX_BIO_CHARS} characters or null"))),
        }
    }
    for field in ["sidebarEntry", "showWhenEmpty"] {
        if let Some(value) = home.get(field) {
            boolean(value, field)?;
        }
    }
    if let Some(host) = home.get("homeHost") {
        short_text(host, "homeHost", MAX_ID_CHARS, true)?;
    }
    match home.get("blocks") {
        None | Some(Value::Null) => {}
        Some(Value::Array(blocks)) if blocks.len() <= MAX_BLOCKS => {
            let mut seen = std::collections::HashSet::new();
            for (index, block) in blocks.iter().enumerate() {
                let field = format!("blocks[{index}]");
                let block = block.as_object().ok_or_else(|| invalid(format!("{field} must be an object {{id, visible}}")))?;
                let id = block.get("id").unwrap_or(&Value::Null);
                short_text(id, &format!("{field}.id"), MAX_SHORT_CHARS, false)?;
                if !matches!(block.get("visible"), Some(Value::Bool(_))) {
                    return Err(invalid(format!("{field}.visible must be a boolean")));
                }
                if !seen.insert(id.as_str().unwrap_or_default().to_string()) {
                    return Err(invalid(format!("{field}.id repeats an earlier block")));
                }
            }
        }
        Some(_) => return Err(invalid(format!("blocks must be an array of at most {MAX_BLOCKS} {{id, visible}} objects or null"))),
    }
    match home.get("pins") {
        None | Some(Value::Null) => {}
        Some(Value::Array(pins)) if pins.len() <= MAX_PINS => {
            for (index, pin) in pins.iter().enumerate() {
                let field = format!("pins[{index}]");
                let pin = pin.as_object().ok_or_else(|| invalid(format!("{field} must be an object {{hostId, kind, key, title?, size?}}")))?;
                short_text(pin.get("hostId").unwrap_or(&Value::Null), &format!("{field}.hostId"), MAX_ID_CHARS, false)?;
                if !pin.get("kind").and_then(Value::as_str).is_some_and(|kind| PIN_KINDS.contains(&kind)) {
                    return Err(invalid(format!("{field}.kind must be one of {}", PIN_KINDS.join(", "))));
                }
                short_text(pin.get("key").unwrap_or(&Value::Null), &format!("{field}.key"), MAX_KEY_CHARS, false)?;
                if pin.get("kind").and_then(Value::as_str) == Some("page")
                    && !pin.get("key").and_then(Value::as_str).is_some_and(valid_page_key)
                {
                    return Err(invalid(format!("{field}.key of a page pin must be <pageId>:<argument> (page id: a-z, 0-9, '.', '_', '-', starting with a letter, at most {MAX_PAGE_ID_CHARS})")));
                }
                if let Some(title) = pin.get("title") {
                    short_text(title, &format!("{field}.title"), MAX_TITLE_CHARS, true)?;
                }
                match pin.get("size") {
                    None | Some(Value::Null) => {}
                    Some(Value::String(size)) if PIN_SIZES.contains(&size.as_str()) => {}
                    Some(_) => return Err(invalid(format!("{field}.size must be one of {} or null", PIN_SIZES.join(", ")))),
                }
            }
        }
        Some(_) => return Err(invalid(format!("pins must be an array of at most {MAX_PINS} pins or null"))),
    }
    Ok(Some(home))
}

/// The cover's codes name the cover, not the avatar whose checks it reuses.
fn cover_error(mut error: UserError) -> UserError {
    error.code = match error.code {
        "invalid_avatar" => "invalid_cover",
        "avatar_too_large" => "cover_too_large",
        code => code,
    };
    error.message = error.message.replace("avatar", "cover");
    error
}

/// Validate a cover upload (`{contentType, data}` already decoded).
pub fn validate_cover(upload: &AvatarUpload) -> Result<(Avatar, Vec<u8>), UserError> {
    validate_image(upload, COVER_RULES).map_err(cover_error)
}

/// Decode `{contentType, data}` (base64) of a cover upload.
pub fn parse_cover_upload(value: &Value) -> Result<AvatarUpload, UserError> {
    crate::management::user_profiles::parse_upload_limited(value, MAX_COVER_BYTES).map_err(cover_error)
}

fn extension(content_type: &str) -> &'static str {
    match content_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        _ => "webp",
    }
}

fn unavailable(error: impl std::fmt::Display) -> UserError {
    UserError::new(503, "users_unavailable", format!("the home page store is unavailable: {error:#}"))
}

#[derive(Clone, Debug)]
pub struct HomeStore {
    /// `runtime/users` (the audit log lives there).
    users_dir: PathBuf,
}

impl HomeStore {
    pub fn open_default() -> anyhow::Result<Self> {
        let home = crate::config::paths::config_dir()?;
        Ok(Self::at(home.join("runtime").join("users")))
    }

    /// `users_dir` = the users runtime directory; the records live in its
    /// `home/` subdirectory.
    pub fn at(users_dir: impl Into<PathBuf>) -> Self {
        Self { users_dir: users_dir.into() }
    }

    pub fn dir(&self) -> PathBuf {
        self.users_dir.join("home")
    }

    fn record_path(&self, user: &str) -> PathBuf {
        self.dir().join(format!("{user}.json"))
    }

    fn cover_path(&self, user: &str, cover: &Avatar) -> PathBuf {
        self.dir().join(format!("{user}-cover-{}.{}", cover.etag, extension(&cover.content_type)))
    }

    fn check_user(user: &str) -> Result<(), UserError> {
        if valid_user_id(user) {
            Ok(())
        } else {
            Err(UserError::new(404, "user_not_found", "no user with this id"))
        }
    }

    /// The stored record; a missing file is the empty record (revision 0).
    pub fn get(&self, user: &str) -> Result<Record, UserError> {
        Self::check_user(user)?;
        match std::fs::read(self.record_path(user)) {
            Ok(bytes) => {
                let record: Record = serde_json::from_slice(&bytes).map_err(unavailable)?;
                if record.schema != SCHEMA {
                    return Err(unavailable("unsupported home page schema"));
                }
                Ok(record)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Record::default()),
            Err(error) => Err(unavailable(error)),
        }
    }

    fn with<T>(&self, user: &str, mutate: impl FnOnce(&mut Record) -> Result<T, UserError>) -> Result<(Record, T), UserError> {
        Self::check_user(user)?;
        let dir = self.dir();
        crate::management::client_tokens::ensure_private_dir(&dir).map_err(unavailable)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join("home.lock"))
            .map_err(unavailable)?;
        lock.lock_exclusive().map_err(unavailable)?;
        let mut record = self.get(user)?;
        let before = record.clone();
        let result = mutate(&mut record)?;
        if record != before {
            crate::config::atomic::write_private_pretty_json_atomic(&self.record_path(user), &record, "user home page")
                .map_err(unavailable)?;
        }
        Ok((record, result))
    }

    /// `PUT /v2/users/me/home`: replace `home` when `revision` is the current
    /// one (409 `home_revision_conflict` `{revision}` otherwise). An unchanged
    /// document keeps its revision.
    pub fn put(&self, user: &str, revision: u64, home: Option<Map<String, Value>>, actor: &str) -> Result<Record, UserError> {
        let (record, changed) = self.with(user, |record| {
            if record.revision != revision {
                let mut error = UserError::new(
                    409,
                    "home_revision_conflict",
                    "the home page changed on another device; reload it and apply the edit again",
                );
                error.details = json!({ "revision": record.revision });
                return Err(error);
            }
            if record.home == home {
                return Ok(false);
            }
            record.home = home;
            record.revision += 1;
            record.updated_at = Some(crate::management::instance::now_utc());
            Ok(true)
        })?;
        if changed {
            audit(
                &self.users_dir,
                "homeUpdated",
                json!({ "userId": user, "actor": actor, "revision": record.revision, "cleared": record.home.is_none() }),
            );
        }
        Ok(record)
    }

    /// `PUT|DELETE /v2/users/me/home/cover`. The image is written before the
    /// record that names it; the replaced one is removed afterwards.
    pub fn set_cover(&self, user: &str, upload: Option<&AvatarUpload>, actor: &str) -> Result<Record, UserError> {
        Self::check_user(user)?;
        let next = upload.map(validate_cover).transpose()?;
        let now = crate::management::instance::now_utc();
        if let Some((cover, bytes)) = &next {
            crate::management::client_tokens::ensure_private_dir(&self.dir())
                .and_then(|_| crate::config::atomic::write_private_bytes_atomic(&self.cover_path(user, cover), bytes))
                .map_err(|e| unavailable(format!("write the cover: {e:#}")))?;
        }
        let next = next.map(|(cover, _)| Avatar { updated_at: now.clone(), ..cover });
        let outcome = self.with(user, |record| {
            if record.cover_etag() == next.as_ref().map(|cover| cover.etag.as_str()) {
                return Ok(None);
            }
            let replaced = record.cover.take();
            record.cover = next.clone();
            record.updated_at = Some(now.clone());
            Ok(Some(replaced))
        });
        let (record, replaced) = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                if let Some(cover) = &next {
                    let _ = std::fs::remove_file(self.cover_path(user, cover));
                }
                return Err(error);
            }
        };
        if let Some(replaced) = replaced {
            if let Some(old) = replaced {
                let _ = std::fs::remove_file(self.cover_path(user, &old));
            }
            audit(
                &self.users_dir,
                "homeCoverUpdated",
                json!({
                    "userId": user,
                    "actor": actor,
                    "cover": record.cover.as_ref().map(|c| json!({ "etag": c.etag, "contentType": c.content_type, "bytes": c.bytes, "width": c.width, "height": c.height })),
                }),
            );
        }
        Ok(record)
    }

    /// The cover bytes of `user`, with its record.
    pub fn cover(&self, user: &str) -> Option<(Avatar, Vec<u8>)> {
        let cover = self.get(user).ok()?.cover?;
        let bytes = std::fs::read(self.cover_path(user, &cover)).ok()?;
        Some((cover, bytes))
    }

    /// Remove a deleted user's home page and cover (best effort).
    pub fn remove(&self, user: &str) {
        if Self::check_user(user).is_err() {
            return;
        }
        if let Ok(Record { cover: Some(cover), .. }) = self.get(user) {
            let _ = std::fs::remove_file(self.cover_path(user, &cover));
        }
        let _ = std::fs::remove_file(self.record_path(user));
    }
}

#[cfg(test)]
#[path = "user_home_tests.rs"]
mod tests;
