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

use crate::management::user_profiles::{
    validate_image, Avatar, AvatarUpload, ImageRules, AVATAR_RULES,
};
use crate::management::users::UserError;

pub const SCHEMA: &str = "cutex/profile-avatars/v1";
pub const MAX_INTRO_PORTRAIT_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_INTRO_PORTRAIT_SIDE: u32 = 1024;
pub const MAX_INTRO_GREETING_CHARS: usize = 300;
pub const MAX_PORTRAIT_THEMES: usize = 128;
pub const MAX_AVATAR_STYLE_BYTES: usize = 8192;

/// Full agent-avatar choice. This never changes the Profile image or writes
/// existing sessions. None on ProfileOptions means the desktop app's default.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum AvatarStyle {
    Portrait {
        #[serde(rename = "portraitThemes")]
        portrait_themes: Vec<String>,
    },
    Emoji {
        // Required even when null: omission must not silently become a default.
        #[serde(rename = "emojiThemes", deserialize_with = "nullable_emoji_themes")]
        emoji_themes: Option<Vec<String>>,
    },
    Letter,
}

fn nullable_emoji_themes<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<String>>, D::Error> {
    Option::<Vec<String>>::deserialize(deserializer)
}

fn portrait_identifier(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 20
        && (id.as_bytes()[0].is_ascii_lowercase() || id.as_bytes()[0].is_ascii_digit())
        && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Validate independently of installed packs. Preserve portrait order and IDs;
/// emoji lists use the same canonical order as legacy avatarThemes.
pub fn normalize_avatar_style(style: &AvatarStyle) -> Result<AvatarStyle, String> {
    if serde_json::to_vec(style).map_err(|_| "invalid avatar style")?.len() > MAX_AVATAR_STYLE_BYTES {
        return Err(format!("at most {MAX_AVATAR_STYLE_BYTES} bytes"));
    }
    match style {
        AvatarStyle::Portrait { portrait_themes } => {
            if portrait_themes.is_empty() || portrait_themes.len() > MAX_PORTRAIT_THEMES {
                return Err(format!("portraitThemes must contain 1-{MAX_PORTRAIT_THEMES} references"));
            }
            let mut seen = std::collections::BTreeSet::new();
            for reference in portrait_themes {
                let valid = reference.split_once('/').is_some_and(|(pack, theme)| {
                    portrait_identifier(pack) && portrait_identifier(theme)
                });
                if !valid { return Err("portraitThemes must use pack/theme identifiers (1-20 lowercase letters, digits or hyphens per segment; first character alphanumeric)".into()); }
                if !seen.insert(reference) { return Err("duplicate portraitThemes reference".into()); }
            }
            Ok(style.clone())
        }
        AvatarStyle::Emoji { emoji_themes } => Ok(AvatarStyle::Emoji {
            emoji_themes: emoji_themes.as_ref().map(|themes| normalize_avatar_themes(themes)).transpose()?,
        }),
        AvatarStyle::Letter => Ok(AvatarStyle::Letter),
    }
}

pub fn parse_avatar_style(value: &serde_json::Value) -> Result<AvatarStyle, String> {
    if value.to_string().len() > MAX_AVATAR_STYLE_BYTES {
        return Err(format!("at most {MAX_AVATAR_STYLE_BYTES} bytes"));
    }
    let object = value.as_object().ok_or("must be a style object or null")?;
    let allowed: &[&str] = match object.get("kind").and_then(serde_json::Value::as_str) {
        Some("portrait") => &["kind", "portraitThemes"],
        Some("emoji") => &["kind", "emojiThemes"],
        Some("letter") => &["kind"],
        _ => return Err("kind must be portrait, emoji or letter".into()),
    };
    if object.len() != allowed.len() || object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err("style requires exactly its declared fields; unknown fields are not allowed".into());
    }
    let style = serde_json::from_value::<AvatarStyle>(value.clone())
        .map_err(|_| "must be portrait {portraitThemes}, emoji {emojiThemes}, or letter; unknown fields are not allowed".to_string())?;
    normalize_avatar_style(&style)
}

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
            return Err(format!(
                "unknown theme {theme:?}; known: {}",
                AVATAR_THEMES.join(", ")
            ));
        }
        if !seen.insert(theme.as_str()) {
            return Err(format!("duplicate theme {theme:?}"));
        }
    }
    Ok(AVATAR_THEMES
        .iter()
        .filter(|id| seen.contains(**id))
        .map(|id| id.to_string())
        .collect())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub schema: String,
    #[serde(default)]
    pub avatars: BTreeMap<String, Avatar>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub intro_portraits: BTreeMap<String, Avatar>,
    /// Per-kind/per-profile clocks, including tombstones after an image DELETE.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub revision_updated_at: BTreeMap<String, BTreeMap<String, String>>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            schema: SCHEMA.into(),
            avatars: BTreeMap::new(),
            intro_portraits: BTreeMap::new(),
            revision_updated_at: BTreeMap::new(),
        }
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
        self.map(kind)
            .get(profile_id)
            .map(|avatar| avatar.etag.as_str())
    }

    pub fn revision(&self, kind: Kind, profile_id: &str) -> serde_json::Value {
        let field = match kind {
            Kind::Avatar => "avatar",
            Kind::IntroPortrait => "introPortrait",
        };
        let image = self.map(kind).get(profile_id);
        let updated_at = if let Some(image) = image {
            (!image.updated_at.is_empty()).then_some(image.updated_at.as_str())
        } else {
            self.revision_updated_at
                .get(field)
                .and_then(|values| values.get(profile_id))
                .map(String::as_str)
        };
        let updated_at = updated_at.and_then(super::appearance::reported_revision_timestamp);
        json!({"etag":image.map(|image| &image.etag),"updatedAt":updated_at})
    }
}

fn unavailable(error: impl std::fmt::Display) -> UserError {
    UserError::new(
        503,
        "profile_avatars_unavailable",
        format!("the profile picture store is unavailable: {error:#}"),
    )
}

/// File stem for a profile id: the id itself when it is a plain token
/// (UUIDs), else a hash of it.
fn stem(profile_id: &str) -> String {
    let plain = !profile_id.is_empty()
        && profile_id.len() <= 64
        && profile_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if plain {
        profile_id.to_string()
    } else {
        let digest = Sha256::digest(profile_id.as_bytes());
        format!(
            "h{}",
            digest
                .iter()
                .take(8)
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        )
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
    format!(
        "{}-{}-{}.{}",
        kind.prefix(),
        stem(profile_id),
        avatar.etag,
        extension(&avatar.content_type)
    )
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
            crate::config::atomic::write_private_pretty_json_atomic(
                &self.path(),
                &state,
                "profile avatars",
            )
            .map_err(unavailable)?;
            // Cleanup stays under the same lock, after the new record is durable. A concurrent
            // writer restoring the old bytes must not have its current image unlinked afterwards.
            for kind in [Kind::Avatar, Kind::IntroPortrait] {
                for (profile_id, old) in before.map(kind) {
                    if !state.map(kind).get(profile_id).is_some_and(|image| {
                        file_name(kind, profile_id, image) == file_name(kind, profile_id, old)
                    }) {
                        let _ = std::fs::remove_file(
                            self.images().join(file_name(kind, profile_id, old)),
                        );
                    }
                }
            }
        }
        Ok(result)
    }

    /// Set (`Some`) or clear (`None`) an image of `profile_id`. Image bytes
    /// are written before the record that names them; the replaced image is
    /// removed afterwards. Returns the stored record (`None` = no image).
    pub fn set(
        &self,
        kind: Kind,
        profile_id: &str,
        upload: Option<&AvatarUpload>,
        actor: &str,
    ) -> Result<Option<Avatar>, UserError> {
        let checked = upload
            .map(|upload| validate_image(upload, kind.rules()))
            .transpose()?;
        let (current, changed) = self.with(|state| -> Result<_, UserError> {
            let previous = state.map(kind).get(profile_id).cloned();
            if previous.as_ref().map(|a| &a.etag) == checked.as_ref().map(|(a, _)| &a.etag) {
                return Ok((previous, false));
            }
            let revision = state.revision(kind, profile_id);
            let now = super::appearance::next_revision_timestamp(revision["updatedAt"].as_str());
            let next = checked.as_ref().map(|(avatar, _)| Avatar {
                updated_at: now.clone(),
                ..avatar.clone()
            });
            if let Some((avatar, bytes)) = &checked {
                let dir = self.images();
                crate::management::client_tokens::ensure_private_dir(&dir)
                    .and_then(|_| {
                        crate::config::atomic::write_private_bytes_atomic(
                            &dir.join(file_name(kind, profile_id, avatar)),
                            bytes,
                        )
                    })
                    .map_err(unavailable)?;
            }
            match &next {
                Some(avatar) => {
                    state
                        .map_mut(kind)
                        .insert(profile_id.to_owned(), avatar.clone());
                }
                None => {
                    state.map_mut(kind).remove(profile_id);
                }
            }
            let field = match kind {
                Kind::Avatar => "avatar",
                Kind::IntroPortrait => "introPortrait",
            };
            state
                .revision_updated_at
                .entry(field.into())
                .or_default()
                .insert(profile_id.into(), now);
            Ok((next, true))
        })??;
        if !changed {
            return Ok(current);
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
        let _ = self.with(|state| {
            for kind in [Kind::Avatar, Kind::IntroPortrait] {
                state.map_mut(kind).remove(profile_id);
            }
            for revisions in state.revision_updated_at.values_mut() {
                revisions.remove(profile_id);
            }
            state
                .revision_updated_at
                .retain(|_, revisions| !revisions.is_empty());
        });
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
    ProfileAvatarStore::open_default()
        .and_then(|store| store.snapshot())
        .unwrap_or_else(|error| {
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

    #[test]
    fn full_avatar_styles_are_strict_canonical_and_preserve_uninstalled_portraits() {
        for value in [
            json!({"kind":"portrait","portraitThemes":["uninstalled-v9/theme-b", "other-pack/theme-a"]}),
            json!({"kind":"emoji","emojiThemes":null}),
            json!({"kind":"letter"}),
        ] {
            let style = parse_avatar_style(&value).unwrap();
            assert_eq!(serde_json::to_value(&style).unwrap(), value);
            assert_eq!(serde_json::from_value::<AvatarStyle>(value).unwrap(), style);
        }
        assert_eq!(serde_json::to_value(parse_avatar_style(&json!({"kind":"emoji","emojiThemes":["cats","sea"]})).unwrap()).unwrap(), json!({"kind":"emoji","emojiThemes":["sea","cats"]}));
        for bad in [
            json!({"kind":"unknown"}), json!({"kind":"emoji"}),
            json!({"kind":"portrait"}), json!({"kind":"letter","emojiThemes":null}),
            json!({"kind":"portrait","portraitThemes":["pack/theme"],"extra":true}),
            json!({"kind":"emoji","emojiThemes":null,"portraitThemes":[]}),
            json!({"kind":"emoji","emojiThemes":[]}),
            json!({"kind":"emoji","emojiThemes":["all"]}),
            json!({"kind":"emoji","emojiThemes":["sea","sea"]}),
            json!({"kind":"portrait","portraitThemes":[]}),
            json!({"kind":"portrait","portraitThemes":["pack/theme","pack/theme"]}),
            json!({"kind":"portrait","portraitThemes": [3]}),
        ] { assert!(parse_avatar_style(&bad).is_err(), "{bad}"); }
        for reference in ["../theme", "pack/../theme", "pack\\theme", "/pack/theme", "https://example.com/theme", "pack/theme.png", "Pack/theme", "pack/theme/extra", "pack/", "-pack/theme", "pack/无", "pack/theme\n"] {
            assert!(parse_avatar_style(&json!({"kind":"portrait","portraitThemes":[reference]})).is_err(), "{reference:?}");
        }
        let full: Vec<_> = (0..MAX_PORTRAIT_THEMES).map(|i| format!("pack-{i}/theme")).collect();
        assert!(parse_avatar_style(&json!({"kind":"portrait","portraitThemes":full})).is_ok());
        let too_many: Vec<_> = (0..=MAX_PORTRAIT_THEMES).map(|i| format!("pack-{i}/theme")).collect();
        assert!(parse_avatar_style(&json!({"kind":"portrait","portraitThemes":too_many})).is_err());
        assert!(parse_avatar_style(&json!({"kind":"portrait","portraitThemes":[format!("{}/theme", "a".repeat(21))]})).is_err());
        assert!(parse_avatar_style(&json!({"kind":"portrait","portraitThemes":["a".repeat(MAX_AVATAR_STYLE_BYTES)]})).is_err());
    }

    fn png(side: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(side, side, image::Rgba([10, 20, 30, 255]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
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
        AvatarUpload {
            content_type: "image/png".into(),
            bytes: png(side),
        }
    }

    #[test]
    fn future_image_and_tombstone_clocks_are_unknown_without_rewriting_state() {
        let mut state = State::default();
        state.avatars.insert(
            "p".into(),
            Avatar {
                etag: "synthetic".into(),
                content_type: "image/png".into(),
                bytes: 1,
                width: 16,
                height: 16,
                updated_at: "2099-01-01T00:00:00Z".into(),
            },
        );
        state
            .revision_updated_at
            .entry("introPortrait".into())
            .or_default()
            .insert("p".into(), "2099-01-01T00:00:00Z".into());
        let before = state.clone();
        for kind in [Kind::Avatar, Kind::IntroPortrait] {
            assert!(state.revision(kind, "p")["updatedAt"].is_null());
        }
        assert_eq!(state, before);
    }
    #[test]
    fn image_revisions_persist_delete_tombstones_and_skip_all_idempotent_writes() {
        let temp = tempdir();
        let store = ProfileAvatarStore::at(temp.path().join("profile-avatars"));
        assert_eq!(
            store.snapshot().unwrap().revision(Kind::Avatar, "p1"),
            json!({"etag":null,"updatedAt":null})
        );
        store.set(Kind::Avatar, "p1", None, "owner").unwrap();
        assert!(
            !store.path().exists(),
            "deleting an already absent image must not invent a clock"
        );
        let first = store
            .set(Kind::Avatar, "p1", Some(&upload(64)), "owner")
            .unwrap()
            .unwrap();
        store
            .set(Kind::IntroPortrait, "p1", Some(&upload(32)), "owner")
            .unwrap();
        let portrait = store
            .snapshot()
            .unwrap()
            .revision(Kind::IntroPortrait, "p1");
        let image = store.images().join(file_name(Kind::Avatar, "p1", &first));
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        for path in [store.path(), image.clone()] {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(old))
                .unwrap();
        }
        let state_bytes = std::fs::read(store.path()).unwrap();
        store
            .set(Kind::Avatar, "p1", Some(&upload(64)), "owner")
            .unwrap();
        assert_eq!(std::fs::read(store.path()).unwrap(), state_bytes);
        assert_eq!(
            std::fs::metadata(store.path()).unwrap().modified().unwrap(),
            old
        );
        assert_eq!(std::fs::metadata(&image).unwrap().modified().unwrap(), old);
        store.set(Kind::Avatar, "p1", None, "owner").unwrap();
        let deleted = store.snapshot().unwrap().revision(Kind::Avatar, "p1");
        assert!(deleted["etag"].is_null());
        assert!(deleted["updatedAt"].as_str().unwrap() > first.updated_at.as_str());
        assert!(!image.exists());
        assert_eq!(
            store
                .snapshot()
                .unwrap()
                .revision(Kind::IntroPortrait, "p1"),
            portrait
        );
        let bytes = std::fs::read(store.path()).unwrap();
        let modified = std::fs::metadata(store.path()).unwrap().modified().unwrap();
        store.set(Kind::Avatar, "p1", None, "owner").unwrap();
        let reopened = ProfileAvatarStore::at(temp.path().join("profile-avatars"));
        assert_eq!(
            reopened.snapshot().unwrap().revision(Kind::Avatar, "p1"),
            deleted
        );
        assert_eq!(std::fs::read(store.path()).unwrap(), bytes);
        assert_eq!(
            std::fs::metadata(store.path()).unwrap().modified().unwrap(),
            modified
        );
        let restored = store
            .set(Kind::Avatar, "p1", Some(&upload(64)), "owner")
            .unwrap()
            .unwrap();
        assert!(restored.updated_at.as_str() > deleted["updatedAt"].as_str().unwrap());
        store.remove("p1");
        assert_eq!(
            store.snapshot().unwrap().revision(Kind::Avatar, "p1"),
            json!({"etag":null,"updatedAt":null})
        );
    }

    #[test]
    fn legacy_image_state_without_revision_metadata_stays_read_only_with_unknown_empty_clock() {
        let state: State = serde_json::from_value(json!({"schema":SCHEMA,"avatars":{"p1":{
            "etag":"legacy","contentType":"image/png","bytes":10,"width":16,"height":16,"updatedAt":""
        }}})).unwrap();
        assert_eq!(
            state.revision(Kind::Avatar, "p1"),
            json!({"etag":"legacy","updatedAt":null})
        );
        assert!(state.revision_updated_at.is_empty());
        let encoded = serde_json::to_vec(&state).unwrap();
        assert_eq!(serde_json::from_slice::<State>(&encoded).unwrap(), state);
    }

    #[test]
    fn concurrent_replacements_keep_the_final_image_bytes_and_revision_consistent() {
        let temp = tempdir();
        let store = ProfileAvatarStore::at(temp.path().join("profile-avatars"));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let workers: Vec<_> = [32, 64]
            .into_iter()
            .map(|side| {
                let store = store.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let image = upload(side);
                    barrier.wait();
                    for _ in 0..20 {
                        store
                            .set(Kind::Avatar, "p1", Some(&image), "owner")
                            .unwrap();
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let (image, bytes) = store
            .image(Kind::Avatar, "p1")
            .expect("current record must always have durable bytes");
        assert_eq!(format!("{:x}", Sha256::digest(bytes))[..16], image.etag);
        assert_eq!(
            store.snapshot().unwrap().revision(Kind::Avatar, "p1"),
            json!({"etag":image.etag,"updatedAt":image.updated_at})
        );
        assert_eq!(std::fs::read_dir(store.images()).unwrap().count(), 1);
    }

    #[test]
    fn set_replace_clear_and_remove() {
        let temp = tempdir();
        let store = ProfileAvatarStore::at(temp.path().join("profile-avatars"));
        assert!(store.image(Kind::Avatar, "p1").is_none());
        let first = store
            .set(Kind::Avatar, "p1", Some(&upload(64)), "owner")
            .unwrap()
            .unwrap();
        assert_eq!(first.etag.len(), 16);
        let (stored, bytes) = store.image(Kind::Avatar, "p1").unwrap();
        assert_eq!(
            (stored.etag.as_str(), bytes),
            (first.etag.as_str(), png(64))
        );
        let image_dir = temp.path().join("profile-avatars").join("images");
        assert_eq!(std::fs::read_dir(&image_dir).unwrap().count(), 1);

        // Same bytes again: unchanged record.
        let again = store
            .set(Kind::Avatar, "p1", Some(&upload(64)), "owner")
            .unwrap()
            .unwrap();
        assert_eq!(again, first);
        // Replace: the old image goes.
        let second = store
            .set(Kind::Avatar, "p1", Some(&upload(32)), "owner")
            .unwrap()
            .unwrap();
        assert_ne!(second.etag, first.etag);
        assert_eq!(std::fs::read_dir(&image_dir).unwrap().count(), 1);
        assert_eq!(
            store.snapshot().unwrap().etag(Kind::Avatar, "p1"),
            Some(second.etag.as_str())
        );

        // A second profile is independent; remove drops only its own.
        store
            .set(Kind::Avatar, "p2", Some(&upload(16)), "owner")
            .unwrap();
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
                .write_to(
                    &mut std::io::Cursor::new(&mut bytes),
                    image::ImageFormat::Png,
                )
                .unwrap();
            AvatarUpload {
                content_type: "image/png".into(),
                bytes,
            }
        };
        let portrait = store
            .set(Kind::IntroPortrait, "p1", Some(&wide(600, 1000)), "owner")
            .unwrap()
            .unwrap();
        assert_eq!((portrait.width, portrait.height), (600, 1000));
        assert!(store.image(Kind::Avatar, "p1").is_none());
        // Not square: refused as a picture; larger than 1024: refused as a portrait.
        assert_eq!(
            store
                .set(Kind::Avatar, "p1", Some(&wide(600, 1000)), "owner")
                .unwrap_err()
                .code,
            "invalid_avatar"
        );
        assert_eq!(
            store
                .set(Kind::IntroPortrait, "p1", Some(&wide(1100, 100)), "owner")
                .unwrap_err()
                .code,
            "invalid_avatar"
        );
        store
            .set(Kind::Avatar, "p1", Some(&upload(64)), "owner")
            .unwrap();
        let state = store.snapshot().unwrap();
        assert_eq!(
            state.etag(Kind::IntroPortrait, "p1"),
            Some(portrait.etag.as_str())
        );
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
        assert_eq!(
            normalize_intro_greeting("  hi {{owner}}  "),
            Ok(Some("hi {{owner}}".into()))
        );
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
        let wrong_type = AvatarUpload {
            content_type: "image/jpeg".into(),
            bytes: png(64),
        };
        assert_eq!(
            store
                .set(Kind::Avatar, "p1", Some(&wrong_type), "owner")
                .unwrap_err()
                .code,
            "invalid_avatar"
        );
        assert_eq!(
            store
                .set(Kind::Avatar, "p1", Some(&upload(600)), "owner")
                .unwrap_err()
                .code,
            "invalid_avatar"
        );
        assert!(!store.path().exists());
    }

    #[test]
    fn odd_profile_ids_get_hashed_file_names() {
        assert_eq!(
            stem("0f1e2d3c-aaaa-bbbb-cccc-001122334455"),
            "0f1e2d3c-aaaa-bbbb-cccc-001122334455"
        );
        let hashed = stem("../weird id");
        assert!(hashed.starts_with('h') && hashed.len() == 17, "{hashed}");
        let temp = tempdir();
        let store = ProfileAvatarStore::at(temp.path().join("profile-avatars"));
        store
            .set(Kind::Avatar, "../weird id", Some(&upload(16)), "owner")
            .unwrap();
        assert!(store.image(Kind::Avatar, "../weird id").is_some());
    }

    #[test]
    fn avatar_themes_are_known_unique_non_empty_and_canonical() {
        let ids = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            normalize_avatar_themes(&ids(&["cats", "animals", "sea"])).unwrap(),
            ids(&["animals", "sea", "cats"])
        );
        assert!(normalize_avatar_themes(&[]).is_err());
        assert!(normalize_avatar_themes(&ids(&["sea", "sea"])).is_err());
        assert!(normalize_avatar_themes(&ids(&["all"])).is_err());
        assert!(normalize_avatar_themes(&ids(&["Animals"])).is_err());
        assert_eq!(
            normalize_avatar_themes(&ids(AVATAR_THEMES)).unwrap(),
            ids(AVATAR_THEMES)
        );
    }
}
