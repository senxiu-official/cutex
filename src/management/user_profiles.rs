//! Self-service user profiles (capability `users-self-profile`): the display
//! name and the small avatar a person sets on their own desktop, published to
//! every host it connects to as that user (cutex-desktop
//! docs/research/users-self-profile-20261014.md).
//!
//! - Store: `<cutex home>/runtime/users/profiles-v1.json` (schema
//!   `cutex/user-profiles/v1`, atomic writes under `profiles.lock`) and the
//!   avatar images in `runtime/users/avatars/<userId>-<etag>.<ext>`. Kept out
//!   of `users-v1.json` on purpose: an older cutex that rewrites the users
//!   store keeps nothing it does not know, but it never touches these files,
//!   so a rollback and roll-forward keeps every profile.
//! - The owner's `name` in the users store stays the owner-set label (a
//!   private alias); clients show `displayName ?? name`.
//! - Avatars: PNG, JPEG or WebP, at most [`MAX_AVATAR_BYTES`], square,
//!   [`MIN_AVATAR_SIDE`]..=[`MAX_AVATAR_SIDE`] px. PNG / JPEG are decoded in
//!   full; WebP (no decoder in this build) is checked structurally (RIFF
//!   size, VP8 / VP8L / VP8X header and dimensions). The ETag is the first 16
//!   hex digits of the bytes' SHA-256.
//! - The audit log records profile changes without image bytes.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use base64::Engine;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::management::users::{audit, UserError, OWNER};

pub const SCHEMA: &str = "cutex/user-profiles/v1";
pub const MAX_AVATAR_BYTES: usize = 48 * 1024;
pub const MIN_AVATAR_SIDE: u32 = 16;
pub const MAX_AVATAR_SIDE: u32 = 512;
const MAX_DISPLAY_NAME_CHARS: usize = 60;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Avatar {
    pub etag: String,
    pub content_type: String,
    pub bytes: usize,
    pub width: u32,
    pub height: u32,
    pub updated_at: String,
}

impl Avatar {
    fn file_name(&self, user: &str) -> String {
        format!("{user}-{}.{}", self.etag, extension(&self.content_type))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<Avatar>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

impl Profile {
    pub fn avatar_etag(&self) -> Option<&str> {
        self.avatar.as_ref().map(|avatar| avatar.etag.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub schema: String,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

impl Default for State {
    fn default() -> Self {
        Self { schema: SCHEMA.into(), profiles: BTreeMap::new() }
    }
}

impl State {
    pub fn profile(&self, user: &str) -> Option<&Profile> {
        self.profiles.get(user)
    }
}

/// What a `PATCH /v2/users/me` changes: `None` = keep, `Some(None)` = clear.
#[derive(Clone, Debug, Default)]
pub struct ProfilePatch {
    pub display_name: Option<Option<String>>,
    pub avatar: Option<Option<AvatarUpload>>,
}

#[derive(Clone, Debug)]
pub struct AvatarUpload {
    pub content_type: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct ProfileStore {
    dir: PathBuf,
}

impl ProfileStore {
    pub fn open_default() -> anyhow::Result<Self> {
        let home = crate::config::paths::config_dir()?;
        Ok(Self::at(home.join("runtime").join("users")))
    }

    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join("profiles-v1.json")
    }

    fn avatars(&self) -> PathBuf {
        self.dir.join("avatars")
    }

    /// Current profiles; a missing file is empty. Errors fail soft at the
    /// callers that only decorate (names fall back to the owner's label).
    pub fn snapshot(&self) -> anyhow::Result<State> {
        match std::fs::read(self.path()) {
            Ok(bytes) => {
                let state: State = serde_json::from_slice(&bytes)?;
                if state.schema != SCHEMA {
                    anyhow::bail!("unsupported user profiles schema");
                }
                Ok(state)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn profile(&self, user: &str) -> Profile {
        self.snapshot().ok().and_then(|state| state.profile(user).cloned()).unwrap_or_default()
    }

    fn with<T>(&self, mutate: impl FnOnce(&mut State) -> Result<T, UserError>) -> Result<T, UserError> {
        let unavailable = |e: anyhow::Error| UserError::new(503, "users_unavailable", format!("the user profiles store is unavailable: {e:#}"));
        crate::management::client_tokens::ensure_private_dir(&self.dir).map_err(unavailable)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.dir.join("profiles.lock"))
            .map_err(|e| unavailable(e.into()))?;
        lock.lock_exclusive().map_err(|e| unavailable(e.into()))?;
        let mut state = self.snapshot().map_err(unavailable)?;
        let before = state.clone();
        let result = mutate(&mut state)?;
        if state != before {
            crate::config::atomic::write_private_pretty_json_atomic(&self.path(), &state, "user profiles")
                .map_err(unavailable)?;
        }
        Ok(result)
    }

    /// Apply a self-service patch to `user`'s profile (the caller decided who
    /// "me" is). Image bytes are written before the record that names them;
    /// the replaced image is removed afterwards.
    pub fn update(&self, user: &str, patch: ProfilePatch, actor: &str) -> Result<Profile, UserError> {
        let display_name = patch.display_name.map(|name| name.map(|n| validate_display_name(&n)).transpose()).transpose()?;
        let avatar = match patch.avatar {
            None => None,
            Some(None) => Some(None),
            Some(Some(upload)) => Some(Some(validate_avatar(&upload)?)),
        };
        let now = crate::management::instance::now_utc();
        if let Some(Some((avatar, bytes))) = &avatar {
            let dir = self.avatars();
            crate::management::client_tokens::ensure_private_dir(&dir)
                .and_then(|_| crate::config::atomic::write_private_bytes_atomic(&dir.join(avatar.file_name(user)), bytes))
                .map_err(|e| UserError::new(503, "users_unavailable", format!("write the avatar: {e:#}")))?;
        }
        let (profile, replaced) = self.with(|state| {
            let profile = state.profiles.entry(user.to_string()).or_default();
            let before = profile.clone();
            if let Some(name) = display_name.clone() {
                profile.display_name = name;
            }
            let mut replaced = None;
            if let Some(avatar) = &avatar {
                let next = avatar.as_ref().map(|(avatar, _)| Avatar { updated_at: now.clone(), ..avatar.clone() });
                if profile.avatar.as_ref().map(|a| &a.etag) != next.as_ref().map(|a| &a.etag) {
                    replaced = profile.avatar.take();
                    profile.avatar = next;
                }
            }
            if *profile != before {
                profile.updated_at = Some(now.clone());
            }
            let profile = profile.clone();
            if profile.display_name.is_none() && profile.avatar.is_none() {
                state.profiles.remove(user);
            }
            Ok((profile, replaced))
        })?;
        if let Some(old) = replaced {
            let _ = std::fs::remove_file(self.avatars().join(old.file_name(user)));
        }
        // Only the fields this patch carried (null = cleared); never image bytes.
        let mut details = json!({ "userId": user, "actor": actor });
        if let Some(name) = &display_name {
            details["displayName"] = json!(name);
        }
        if let Some(avatar) = &avatar {
            details["avatar"] = match avatar {
                Some((avatar, _)) => json!({ "etag": avatar.etag, "contentType": avatar.content_type, "bytes": avatar.bytes, "width": avatar.width, "height": avatar.height }),
                None => Value::Null,
            };
        }
        audit(&self.dir, "profileUpdated", details);
        Ok(profile)
    }

    /// Remove a deleted user's profile and image (best effort).
    pub fn remove(&self, user: &str) {
        let removed = self.with(|state| Ok(state.profiles.remove(user)));
        if let Ok(Some(Profile { avatar: Some(avatar), .. })) = removed {
            let _ = std::fs::remove_file(self.avatars().join(avatar.file_name(user)));
        }
    }

    /// The avatar bytes of `user`, with its record.
    pub fn avatar(&self, user: &str) -> Option<(Avatar, Vec<u8>)> {
        let avatar = self.snapshot().ok()?.profile(user)?.avatar.clone()?;
        let bytes = std::fs::read(self.avatars().join(avatar.file_name(user))).ok()?;
        Some((avatar, bytes))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

fn extension(content_type: &str) -> &'static str {
    match content_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        _ => "webp",
    }
}

pub fn validate_display_name(name: &str) -> Result<String, UserError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_DISPLAY_NAME_CHARS || name.chars().any(char::is_control) {
        return Err(UserError::invalid(format!(
            "displayName: 1..={MAX_DISPLAY_NAME_CHARS} characters, no control characters"
        )));
    }
    Ok(name.to_string())
}

fn invalid_avatar(message: impl Into<String>) -> UserError {
    UserError::new(400, "invalid_avatar", message)
}

/// Decode `{contentType, data}` (base64) from a request body.
pub fn parse_upload(value: &Value) -> Result<AvatarUpload, UserError> {
    let object = value.as_object().ok_or_else(|| invalid_avatar("avatar must be {contentType, data} or null"))?;
    if let Some(key) = object.keys().find(|k| !matches!(k.as_str(), "contentType" | "data")) {
        return Err(invalid_avatar(format!("avatar: unsupported field {key}")));
    }
    let content_type = object.get("contentType").and_then(Value::as_str).ok_or_else(|| invalid_avatar("avatar.contentType is required"))?;
    let data = object.get("data").and_then(Value::as_str).ok_or_else(|| invalid_avatar("avatar.data (base64) is required"))?;
    if data.len() > MAX_AVATAR_BYTES.div_ceil(3) * 4 + 4 {
        return Err(UserError::new(413, "avatar_too_large", format!("the avatar may be at most {MAX_AVATAR_BYTES} bytes")));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .map_err(|_| invalid_avatar("avatar.data is not valid base64"))?;
    Ok(AvatarUpload { content_type: content_type.to_string(), bytes })
}

/// Check type (declared and sniffed), size and dimensions; returns the record
/// to store and the bytes.
pub fn validate_avatar(upload: &AvatarUpload) -> Result<(Avatar, Vec<u8>), UserError> {
    let bytes = &upload.bytes;
    if bytes.is_empty() {
        return Err(invalid_avatar("the avatar is empty"));
    }
    if bytes.len() > MAX_AVATAR_BYTES {
        return Err(UserError::new(413, "avatar_too_large", format!("the avatar may be at most {MAX_AVATAR_BYTES} bytes")));
    }
    let sniffed = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        "image/jpeg"
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        return Err(invalid_avatar("the avatar must be a PNG, JPEG or WebP image"));
    };
    if upload.content_type != sniffed {
        return Err(invalid_avatar(format!("avatar.contentType {} does not match the image ({sniffed})", upload.content_type)));
    }
    let (width, height) = match sniffed {
        "image/webp" => webp_dimensions(bytes).ok_or_else(|| invalid_avatar("the WebP image is malformed"))?,
        _ => {
            let format = if sniffed == "image/png" { image::ImageFormat::Png } else { image::ImageFormat::Jpeg };
            let mut reader = image::ImageReader::with_format(Cursor::new(bytes.as_slice()), format);
            let mut limits = image::Limits::default();
            limits.max_image_width = Some(MAX_AVATAR_SIDE);
            limits.max_image_height = Some(MAX_AVATAR_SIDE);
            limits.max_alloc = Some(8 * 1024 * 1024);
            reader.limits(limits);
            let decoded = reader.decode().map_err(|_| invalid_avatar("the avatar image could not be decoded"))?;
            (decoded.width(), decoded.height())
        }
    };
    if width != height || !(MIN_AVATAR_SIDE..=MAX_AVATAR_SIDE).contains(&width) {
        return Err(invalid_avatar(format!(
            "the avatar must be square, {MIN_AVATAR_SIDE}..={MAX_AVATAR_SIDE} px (got {width}×{height})"
        )));
    }
    let digest = Sha256::digest(bytes);
    let etag: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    Ok((
        Avatar {
            etag,
            content_type: sniffed.to_string(),
            bytes: bytes.len(),
            width,
            height,
            updated_at: String::new(),
        },
        bytes.clone(),
    ))
}

/// Structural WebP check: RIFF size, first chunk and its dimensions.
fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let u32le = |at: usize| bytes.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let u24le = |at: usize| bytes.get(at..at + 3).map(|b| u32::from(b[0]) | u32::from(b[1]) << 8 | u32::from(b[2]) << 16);
    let riff = u32le(4)? as usize;
    if riff + 8 != bytes.len() || bytes.len() < 30 {
        return None;
    }
    let chunk = bytes.get(12..16)?;
    let chunk_len = u32le(16)? as usize;
    if 20 + chunk_len > bytes.len() {
        return None;
    }
    match chunk {
        b"VP8 " => {
            if bytes.get(23..26)? != [0x9d, 0x01, 0x2a] {
                return None;
            }
            let w = u32::from(u16::from_le_bytes([bytes[26], bytes[27]]) & 0x3fff);
            let h = u32::from(u16::from_le_bytes([bytes[28], bytes[29]]) & 0x3fff);
            Some((w, h))
        }
        b"VP8L" => {
            if *bytes.get(20)? != 0x2f {
                return None;
            }
            let bits = u32le(21)?;
            Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
        }
        b"VP8X" => Some((u24le(24)? + 1, u24le(27)? + 1)),
        _ => None,
    }
}

/// The profile fields every user view carries.
pub fn view_fields(profile: Option<&Profile>) -> Value {
    json!({
        "displayName": profile.and_then(|p| p.display_name.clone()),
        "avatarEtag": profile.and_then(|p| p.avatar_etag().map(str::to_string)),
    })
}

/// Merge `displayName` / `avatarEtag` into a user view object.
pub fn decorate(value: &mut Value, profile: Option<&Profile>) {
    if let (Some(object), Value::Object(fields)) = (value.as_object_mut(), view_fields(profile)) {
        object.extend(fields);
    }
}

/// `decorate` from the default store (best effort: no fields on errors).
pub fn decorate_default(value: &mut Value, user: &str) {
    let profile = ProfileStore::open_default().ok().map(|store| store.profile(user));
    decorate(value, profile.as_ref());
}

/// "me" for a principal: the owner record for the owner, the bound user
/// otherwise; `None` for pending.
pub fn me(principal: Option<&crate::management::users::Principal>) -> Option<String> {
    use crate::management::users::Principal;
    match principal {
        None | Some(Principal::Owner) => Some(OWNER.into()),
        Some(Principal::User { id, .. }) => Some(id.clone()),
        Some(Principal::Pending) => None,
    }
}

#[cfg(test)]
#[path = "user_profiles_tests.rs"]
mod tests;
