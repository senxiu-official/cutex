use super::*;

fn png(side: u32) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(side, side).write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

fn jpeg(width: u32, height: u32) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(width, height).write_to(&mut out, image::ImageFormat::Jpeg).unwrap();
    out.into_inner()
}

/// A structurally valid lossless WebP header (VP8L) of `side`×`side`.
fn webp(side: u32) -> Vec<u8> {
    let bits = (side - 1) | ((side - 1) << 14);
    let mut payload = vec![0x2f];
    payload.extend_from_slice(&bits.to_le_bytes());
    payload.extend_from_slice(&[0; 7]);
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&((4 + 8 + payload.len()) as u32).to_le_bytes());
    out.extend_from_slice(b"WEBPVP8L");
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

fn upload(content_type: &str, bytes: Vec<u8>) -> AvatarUpload {
    AvatarUpload { content_type: content_type.into(), bytes }
}

fn temp() -> (PathBuf, ProfileStore) {
    let root = std::env::temp_dir().join(format!("cutex-profiles-{}", uuid::Uuid::new_v4()));
    (root.clone(), ProfileStore::at(root.join("users")))
}

#[test]
fn avatars_are_checked_for_type_size_and_dimensions() {
    let (avatar, _) = validate_avatar(&upload("image/png", png(128))).unwrap();
    assert_eq!((avatar.width, avatar.height, avatar.content_type.as_str()), (128, 128, "image/png"));
    assert_eq!(avatar.etag.len(), 16);
    let (avatar, _) = validate_avatar(&upload("image/jpeg", jpeg(64, 64))).unwrap();
    assert_eq!(avatar.width, 64);
    let (avatar, _) = validate_avatar(&upload("image/webp", webp(128))).unwrap();
    assert_eq!((avatar.width, avatar.height), (128, 128));

    let code = |upload: AvatarUpload| validate_avatar(&upload).unwrap_err().code;
    assert_eq!(code(upload("image/webp", png(128))), "invalid_avatar", "declared type must match");
    assert_eq!(code(upload("image/png", jpeg(64, 32))), "invalid_avatar");
    assert_eq!(code(upload("image/jpeg", jpeg(64, 32))), "invalid_avatar", "must be square");
    assert_eq!(code(upload("image/png", png(8))), "invalid_avatar", "too small");
    assert_eq!(code(upload("image/png", png(600))), "invalid_avatar", "too large");
    assert_eq!(code(upload("image/gif", b"GIF89a....".to_vec())), "invalid_avatar");
    assert_eq!(code(upload("image/png", Vec::new())), "invalid_avatar");
    assert_eq!(code(upload("image/png", vec![0; MAX_AVATAR_BYTES + 1])), "avatar_too_large");
    let mut truncated = png(128);
    truncated.truncate(truncated.len() / 2);
    assert_eq!(code(upload("image/png", truncated)), "invalid_avatar", "PNG is decoded in full");
    let mut bad_riff = webp(128);
    bad_riff.push(0);
    assert_eq!(code(upload("image/webp", bad_riff)), "invalid_avatar", "RIFF size must match");

    let data = base64::engine::general_purpose::STANDARD.encode(png(32));
    assert!(parse_upload(&json!({"contentType": "image/png", "data": data})).is_ok());
    assert_eq!(parse_upload(&json!({"contentType": "image/png", "data": "!!"})).unwrap_err().code, "invalid_avatar");
    assert_eq!(parse_upload(&json!({"contentType": "image/png", "data": "", "x": 1})).unwrap_err().code, "invalid_avatar");
    let huge = "A".repeat(MAX_AVATAR_BYTES * 2);
    assert_eq!(parse_upload(&json!({"contentType": "image/png", "data": huge})).unwrap_err().code, "avatar_too_large");
}

#[test]
fn profiles_update_replace_clear_and_remove() {
    let (root, store) = temp();
    assert_eq!(store.profile("owner"), Profile::default());
    let patch = ProfilePatch {
        display_name: Some(Some("  Riley ".into())),
        avatar: Some(Some(upload("image/png", png(128)))),
    };
    let first = store.update("owner", patch, "root").unwrap();
    assert_eq!(first.display_name.as_deref(), Some("Riley"));
    let first_avatar = first.avatar.clone().unwrap();
    let (avatar, bytes) = store.avatar("owner").unwrap();
    assert_eq!(avatar.etag, first_avatar.etag);
    assert_eq!(bytes, png(128));

    // Replacing the image removes the old file; a name-only patch keeps it.
    let second = store
        .update("owner", ProfilePatch { avatar: Some(Some(upload("image/jpeg", jpeg(96, 96)))), ..Default::default() }, "root")
        .unwrap();
    assert_ne!(second.avatar_etag(), first.avatar_etag());
    assert_eq!(second.display_name.as_deref(), Some("Riley"));
    let files: Vec<_> = std::fs::read_dir(store.dir().join("avatars")).unwrap().collect();
    assert_eq!(files.len(), 1, "the replaced avatar file is removed");
    let third = store.update("owner", ProfilePatch { display_name: Some(Some("J".into())), ..Default::default() }, "root").unwrap();
    assert_eq!(third.avatar_etag(), second.avatar_etag());

    // Validation leaves the stored profile untouched.
    let bad = store.update("owner", ProfilePatch { display_name: Some(Some("\u{7}".into())), ..Default::default() }, "root");
    assert_eq!(bad.unwrap_err().code, "invalid_request");
    assert_eq!(store.profile("owner").display_name.as_deref(), Some("J"));

    // Clearing both empties the record.
    store
        .update("owner", ProfilePatch { display_name: Some(None), avatar: Some(None) }, "root")
        .unwrap();
    assert!(store.snapshot().unwrap().profiles.is_empty());
    assert!(store.avatar("owner").is_none());

    store
        .update("u-0123456789ab", ProfilePatch { avatar: Some(Some(upload("image/png", png(64)))), ..Default::default() }, "mct-1")
        .unwrap();
    store.remove("u-0123456789ab");
    assert!(store.avatar("u-0123456789ab").is_none());
    assert_eq!(std::fs::read_dir(store.dir().join("avatars")).unwrap().count(), 0);

    // The audit log names changes, never image bytes.
    let audit = std::fs::read_to_string(store.dir().join("audit.jsonl")).unwrap();
    assert!(audit.contains("profileUpdated"));
    let encoded = base64::engine::general_purpose::STANDARD.encode(png(128));
    assert!(!audit.contains(&encoded[..40]));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn views_carry_display_name_and_etag() {
    let mut value = json!({"id": "owner", "name": "Owner"});
    decorate(&mut value, None);
    assert_eq!(value, json!({"id": "owner", "name": "Owner", "displayName": null, "avatarEtag": null}));
    let profile = Profile { display_name: Some("Riley".into()), ..Default::default() };
    decorate(&mut value, Some(&profile));
    assert_eq!(value["displayName"], "Riley");
    use crate::management::users::{Grant, Principal};
    assert_eq!(me(None).as_deref(), Some(OWNER));
    assert_eq!(me(Some(&Principal::Owner)).as_deref(), Some(OWNER));
    assert_eq!(me(Some(&Principal::Pending)), None);
    let user = Principal::User { id: "u-0123456789ab".into(), name: "B".into(), grant: Grant::default() };
    assert_eq!(me(Some(&user)).as_deref(), Some("u-0123456789ab"));
}
