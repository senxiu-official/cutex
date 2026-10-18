//! Profile looks stored on the host (capability `profile-avatar`), so every
//! device that connects shows the same ones:
//!
//! - the Profile **picture** ([`Kind::Avatar`]): the small round image of
//!   the Profile; same rules as user avatars (`management::user_profiles`):
//!   PNG / JPEG / WebP, at most 48 KiB, square, 16..=512 px;
//! - the **intro portrait** ([`Kind::IntroPortrait`]): the opening artwork
//!   above a conversation's first turn; PNG / JPEG / WebP, at most
//!   [`MAX_INTRO_PORTRAIT_BYTES`], any aspect, each side
//!   1..=[`MAX_INTRO_PORTRAIT_SIDE`] px;
//! - **avatar themes** (`options.avatar_themes` in `accounts.json`,
//!   [`AVATAR_THEMES`]) and the **intro greeting** (`options.intro_greeting`,
//!   at most [`MAX_INTRO_GREETING_CHARS`]): small values next to `color`.
//!
//! Images: `<cutex home>/runtime/profile-avatars/avatars-v1.json` (schema
//! `cutex/profile-avatars/v1`, atomic writes under `avatars.lock`) and the
//! bytes in `runtime/profile-avatars/images/<kind>-<stem>-<etag>.<ext>`.
//! Keyed by the profile **id** (a rename keeps them). Kept out of
//! `accounts.json` / `config.json`: an older cutex never reads or rewrites
//! these files, so a rollback and roll-forward keeps every image. The ETag is
//! the first 16 hex digits of the bytes' SHA-256. Profile delete removes the
//! images; duplicate / export / import do not copy them.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::PathBuf;

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::management::user_profiles::{validate_image, Avatar, AvatarUpload, ImageRules, AVATAR_RULES};
use crate::management::users::UserError;

pub const SCHEMA: &str = "cutex/profile-avatars/v1";
pub const MAX_INTRO_PORTRAIT_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_INTRO_PORTRAIT_SIDE: u32 = 1024;
pub const MAX_INTRO_GREETING_CHARS: usize = 300;

/// The two images a Profile may carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Avatar,
    IntroPortrait,
}

impl Kind {
    pub fn rules(self) -> ImageRules {
        match self {
            Kind::Avatar => AVATAR_RULES,
            Kind::IntroPortrait => ImageRules {
                max_bytes: MAX_INTRO_PORTRAIT_BYTES,
                min_side: 1,
                max_side: MAX_INTRO_PORTRAIT_SIDE,
                square: false,
            },
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Kind::Avatar => "avatar",
            Kind::IntroPortrait => "intro",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Avatar => "picture",
            Kind::IntroPortrait => "intro portrait",
        }
    }
}

/// Trimmed greeting (at most [`MAX_INTRO_GREETING_CHARS`], no control
/// characters except line breaks); `None` = empty (clear).
pub fn normalize_intro_greeting(text: &str) -> Result<Option<String>, String> {
    let text = text.trim().replace("\r\n", "\n");
    if text.is_empty() {
        return Ok(None);
    }
    if text.chars().count() > MAX_INTRO_GREETING_CHARS {
        return Err(format!("at most {MAX_INTRO_GREETING_CHARS} characters"));
    }
    if text.chars().any(|c| c.is_control() && c != '\n') {
        return Err("control characters are not allowed (line breaks are)".into());
    }
    Ok(Some(text))
}

/// Known avatar theme ids, in canonical order (clients pick from the union of
/// the selected themes in this order). Extend here; an id is never removed
/// while a stored profile may name it.
pub const AVATAR_THEMES: &[&str] = &[
    "animals",
    "sea",
    "gems",
    "plants",
    "food",
    "sky",
    "birds",
    "insects",
    "fantasy",
    "prehistoric",
    "desserts",
    "seasons",
    "transport",
    "music",
    "cats",
];

/// Validate an `avatarThemes` list: non-empty, unique, known ids. Returns the
/// ids in canonical ([`AVATAR_THEMES`]) order.
pub fn normalize_avatar_themes(themes: &[String]) -> Result<Vec<String>, String> {
    if themes.is_empty() {
        return Err("must list at least one theme (null = the default)".into());
    }
    let mut seen = std::collections::BTreeSet::new();
    for theme in themes {
        if !AVATAR_THEMES.contains(&theme.as_str()) {
            return Err(format!("unknown theme {theme:?}; known: {}", AVATAR_THEMES.join(", ")));
        }
        if !seen.insert(theme.as_str()) {
            return Err(format!("duplicate theme {theme:?}"));
        }
    }
    Ok(AVATAR_THEMES.iter().filter(|id| seen.contains(**id)).map(|id| id.to_string()).collect())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub schema: String,
    #[serde(default)]
    pub avatars: BTreeMap<String, Avatar>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub intro_portraits: BTreeMap<String, Avatar>,
}

impl Default for State {
    fn default() -> Self {
        Self { schema: SCHEMA.into(), avatars: BTreeMap::new(), intro_portraits: BTreeMap::new() }
    }
}

impl State {
    fn map(&self, kind: Kind) -> &BTreeMap<String, Avatar> {
        match kind {
            Kind::Avatar => &self.avatars,
            Kind::IntroPortrait => &self.intro_portraits,
        }
    }

    fn map_mut(&mut self, kind: Kind) -> &mut BTreeMap<String, Avatar> {
        match kind {
            Kind::Avatar => &mut self.avatars,
            Kind::IntroPortrait => &mut self.intro_portraits,
        }
    }

    pub fn etag(&self, kind: Kind, profile_id: &str) -> Option<&str> {
        self.map(kind).get(profile_id).map(|avatar| avatar.etag.as_str())
    }
}

fn unavailable(error: impl std::fmt::Display) -> UserError {
    UserError::new(503, "profile_avatars_unavailable", format!("the profile picture store is unavailable: {error:#}"))
}

/// File stem for a profile id: the id itself when it is a plain token
/// (UUIDs), else a hash of it.
fn stem(profile_id: &str) -> String {
    let plain = !profile_id.is_empty()
        && profile_id.len() <= 64
        && profile_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if plain {
        profile_id.to_string()
    } else {
        let digest = Sha256::digest(profile_id.as_bytes());
        format!("h{}", digest.iter().take(8).map(|b| format!("{b:02x}")).collect::<String>())
    }
}

fn extension(content_type: &str) -> &'static str {
    match content_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        _ => "webp",
    }
}

fn file_name(kind: Kind, profile_id: &str, avatar: &Avatar) -> String {
    format!("{}-{}-{}.{}", kind.prefix(), stem(profile_id), avatar.etag, extension(&avatar.content_type))
}

#[derive(Clone, Debug)]
pub struct ProfileAvatarStore {
    dir: PathBuf,
}

impl ProfileAvatarStore {
    pub fn open_default() -> anyhow::Result<Self> {
        let home = crate::config::paths::config_dir()?;
        Ok(Self::at(home.join("runtime").join("profile-avatars")))
    }

    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join("avatars-v1.json")
    }

    fn images(&self) -> PathBuf {
        self.dir.join("images")
    }

    /// Current pictures; a missing file is empty.
    pub fn snapshot(&self) -> anyhow::Result<State> {
        match std::fs::read(self.path()) {
            Ok(bytes) => {
                let state: State = serde_json::from_slice(&bytes)?;
                anyhow::ensure!(state.schema == SCHEMA, "unsupported profile avatars schema");
                Ok(state)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(error) => Err(error.into()),
        }
    }

    fn with<T>(&self, mutate: impl FnOnce(&mut State) -> T) -> Result<T, UserError> {
        crate::management::client_tokens::ensure_private_dir(&self.dir).map_err(unavailable)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.dir.join("avatars.lock"))
            .map_err(unavailable)?;
        lock.lock_exclusive().map_err(unavailable)?;
        let mut state = self.snapshot().map_err(unavailable)?;
        let before = state.clone();
        let result = mutate(&mut state);
        if state != before {
            crate::config::atomic::write_private_pretty_json_atomic(&self.path(), &state, "profile avatars")
                .map_err(unavailable)?;
        }
        Ok(result)
    }

    /// Set (`Some`) or clear (`None`) an image of `profile_id`. Image bytes
    /// are written before the record that names them; the replaced image is
    /// removed afterwards. Returns the stored record (`None` = no image).
    pub fn set(&self, kind: Kind, profile_id: &str, upload: Option<&AvatarUpload>, actor: &str) -> Result<Option<Avatar>, UserError> {
        let checked = upload.map(|upload| validate_image(upload, kind.rules())).transpose()?;
        let now = crate::management::instance::now_utc();
        if let Some((avatar, bytes)) = &checked {
            let dir = self.images();
            crate::management::client_tokens::ensure_private_dir(&dir)
                .and_then(|_| crate::config::atomic::write_private_bytes_atomic(&dir.join(file_name(kind, profile_id, avatar)), bytes))
                .map_err(unavailable)?;
        }
        let next = checked.as_ref().map(|(avatar, _)| Avatar { updated_at: now.clone(), ..avatar.clone() });
        let (current, replaced) = self.with(|state| {
            let map = state.map_mut(kind);
            let previous = map.get(profile_id).cloned();
            if previous.as_ref().map(|a| &a.etag) == next.as_ref().map(|a| &a.etag) {
                return (previous, None);
            }
            match &next {
                Some(avatar) => map.insert(profile_id.to_string(), avatar.clone()),
                None => map.remove(profile_id),
            };
            (next.clone(), previous)
        })?;
        if let Some(old) = replaced {
            let _ = std::fs::remove_file(self.images().join(file_name(kind, profile_id, &old)));
        }
        crate::management::users::audit(
            &self.dir,
            match kind {
                Kind::Avatar => "profileAvatarUpdated",
                Kind::IntroPortrait => "profileIntroPortraitUpdated",
            },
            json!({
                "profileId": profile_id,
                "actor": actor,
                "avatar": current.as_ref().map(|a| json!({ "etag": a.etag, "contentType": a.content_type, "bytes": a.bytes, "width": a.width, "height": a.height })),
            }),
        );
        Ok(current)
    }

    /// Remove a deleted profile's images (best effort).
    pub fn remove(&self, profile_id: &str) {
        if !self.path().exists() {
            return;
        }
        let removed = self.with(|state| {
            [Kind::Avatar, Kind::IntroPortrait]
                .map(|kind| state.map_mut(kind).remove(profile_id).map(|avatar| (kind, avatar)))
        });
        for (kind, avatar) in removed.into_iter().flatten().flatten() {
            let _ = std::fs::remove_file(self.images().join(file_name(kind, profile_id, &avatar)));
        }
    }

    /// The image bytes of `profile_id`, with its record.
    pub fn image(&self, kind: Kind, profile_id: &str) -> Option<(Avatar, Vec<u8>)> {
        let avatar = self.snapshot().ok()?.map(kind).get(profile_id)?.clone();
        let bytes = std::fs::read(self.images().join(file_name(kind, profile_id, &avatar))).ok()?;
        Some((avatar, bytes))
    }
}

/// Image ETags of every profile from the default store (best effort: empty
/// on errors, so listings never fail on a damaged picture store).
pub fn default_snapshot() -> State {
    ProfileAvatarStore::open_default().and_then(|store| store.snapshot()).unwrap_or_else(|error| {
        eprintln!("profile avatars unavailable; listing profiles without pictures: {error:#}");
        State::default()
    })
}

/// Best-effort cleanup after a profile delete (both images).
pub fn remove_default(profile_id: &str) {
    if let Ok(store) = ProfileAvatarStore::open_default() {
        store.remove(profile_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(side: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(side, side, image::Rgba([10, 20, 30, 255]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        bytes
    }

    /// A private temp dir removed on drop.
    struct Temp(PathBuf);
    impl Temp {
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tempdir() -> Temp {
        Temp(std::env::temp_dir().join(format!("cutex-profile-avatars-{}", uuid::Uuid::new_v4())))
    }

    fn upload(side: u32) -> AvatarUpload {
        AvatarUpload { content_type: "image/png".into(), bytes: png(side) }
    }

    #[test]
    fn set_replace_clear_and_remove() {
        let temp = tempdir();
        let store = ProfileAvatarStore::at(temp.path().join("profile-avatars"));
        assert!(store.image(Kind::Avatar, "p1").is_none());
        let first = store.set(Kind::Avatar, "p1", Some(&upload(64)), "owner").unwrap().unwrap();
        assert_eq!(first.etag.len(), 16);
        let (stored, bytes) = store.image(Kind::Avatar, "p1").unwrap();
        assert_eq!((stored.etag.as_str(), bytes), (first.etag.as_str(), png(64)));
        let image_dir = temp.path().join("profile-avatars").join("images");
        assert_eq!(std::fs::read_dir(&image_dir).unwrap().count(), 1);

        // Same bytes again: unchanged record.
        let again = store.set(Kind::Avatar, "p1", Some(&upload(64)), "owner").unwrap().unwrap();
        assert_eq!(again, first);
        // Replace: the old image goes.
        let second = store.set(Kind::Avatar, "p1", Some(&upload(32)), "owner").unwrap().unwrap();
        assert_ne!(second.etag, first.etag);
        assert_eq!(std::fs::read_dir(&image_dir).unwrap().count(), 1);
        assert_eq!(store.snapshot().unwrap().etag(Kind::Avatar, "p1"), Some(second.etag.as_str()));

        // A second profile is independent; remove drops only its own.
        store.set(Kind::Avatar, "p2", Some(&upload(16)), "owner").unwrap();
        store.remove("p1");
        assert!(store.image(Kind::Avatar, "p1").is_none());
        assert!(store.image(Kind::Avatar, "p2").is_some());
        assert_eq!(std::fs::read_dir(&image_dir).unwrap().count(), 1);

        // Clear.
        assert_eq!(store.set(Kind::Avatar, "p2", None, "owner").unwrap(), None);
        assert!(store.snapshot().unwrap().avatars.is_empty());
        assert_eq!(std::fs::read_dir(&image_dir).unwrap().count(), 0);
    }

    #[test]
    fn intro_portraits_are_separate_and_need_not_be_square() {
        let temp = tempdir();
        let store = ProfileAvatarStore::at(temp.path().join("profile-avatars"));
        let wide = |w: u32, h: u32| {
            let image = image::RgbaImage::from_pixel(w, h, image::Rgba([1, 2, 3, 255]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgba8(image)
                .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
                .unwrap();
            AvatarUpload { content_type: "image/png".into(), bytes }
        };
        let portrait = store.set(Kind::IntroPortrait, "p1", Some(&wide(600, 1000)), "owner").unwrap().unwrap();
        assert_eq!((portrait.width, portrait.height), (600, 1000));
        assert!(store.image(Kind::Avatar, "p1").is_none());
        // Not square: refused as a picture; larger than 1024: refused as a portrait.
        assert_eq!(store.set(Kind::Avatar, "p1", Some(&wide(600, 1000)), "owner").unwrap_err().code, "invalid_avatar");
        assert_eq!(store.set(Kind::IntroPortrait, "p1", Some(&wide(1100, 100)), "owner").unwrap_err().code, "invalid_avatar");
        store.set(Kind::Avatar, "p1", Some(&upload(64)), "owner").unwrap();
        let state = store.snapshot().unwrap();
        assert_eq!(state.etag(Kind::IntroPortrait, "p1"), Some(portrait.etag.as_str()));
        assert!(state.etag(Kind::Avatar, "p1").is_some());
        // Delete removes both.
        store.remove("p1");
        let state = store.snapshot().unwrap();
        assert!(state.avatars.is_empty() && state.intro_portraits.is_empty());
        let image_dir = temp.path().join("profile-avatars").join("images");
        assert_eq!(std::fs::read_dir(&image_dir).unwrap().count(), 0);
    }

    #[test]
    fn intro_greeting_rules() {
        assert_eq!(normalize_intro_greeting("  hi {{owner}}  "), Ok(Some("hi {{owner}}".into())));
        assert_eq!(normalize_intro_greeting("a\r\nb"), Ok(Some("a\nb".into())));
        assert_eq!(normalize_intro_greeting("   "), Ok(None));
        assert!(normalize_intro_greeting("bad\u{7}").is_err());
        assert!(normalize_intro_greeting(&"字".repeat(301)).is_err());
        assert!(normalize_intro_greeting(&"字".repeat(300)).is_ok());
    }

    #[test]
    fn invalid_images_are_refused_without_writes() {
        let temp = tempdir();
        let store = ProfileAvatarStore::at(temp.path().join("profile-avatars"));
        let wrong_type = AvatarUpload { content_type: "image/jpeg".into(), bytes: png(64) };
        assert_eq!(store.set(Kind::Avatar, "p1", Some(&wrong_type), "owner").unwrap_err().code, "invalid_avatar");
        assert_eq!(store.set(Kind::Avatar, "p1", Some(&upload(600)), "owner").unwrap_err().code, "invalid_avatar");
        assert!(!store.path().exists());
    }

    #[test]
    fn odd_profile_ids_get_hashed_file_names() {
        assert_eq!(stem("0f1e2d3c-aaaa-bbbb-cccc-001122334455"), "0f1e2d3c-aaaa-bbbb-cccc-001122334455");
        let hashed = stem("../weird id");
        assert!(hashed.starts_with('h') && hashed.len() == 17, "{hashed}");
        let temp = tempdir();
        let store = ProfileAvatarStore::at(temp.path().join("profile-avatars"));
        store.set(Kind::Avatar, "../weird id", Some(&upload(16)), "owner").unwrap();
        assert!(store.image(Kind::Avatar, "../weird id").is_some());
    }

    #[test]
    fn avatar_themes_are_known_unique_non_empty_and_canonical() {
        let ids = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(normalize_avatar_themes(&ids(&["cats", "animals", "sea"])).unwrap(), ids(&["animals", "sea", "cats"]));
        assert!(normalize_avatar_themes(&[]).is_err());
        assert!(normalize_avatar_themes(&ids(&["sea", "sea"])).is_err());
        assert!(normalize_avatar_themes(&ids(&["all"])).is_err());
        assert!(normalize_avatar_themes(&ids(&["Animals"])).is_err());
        assert_eq!(normalize_avatar_themes(&ids(AVATAR_THEMES)).unwrap(), ids(AVATAR_THEMES));
    }
}
