//! Read-only metadata and lookup helpers for durable `cutex_session` records.

use std::path::Path;

use crate::agent_bus::model::AgentRegistrationClass;
use crate::session::identity::default_cutex_session_id_for_codex_session;
use crate::session::model::CutexSessionRecord;
use crate::session::model::CutexSessionStore;

pub fn cutex_session_launch_cwd(record: &CutexSessionRecord) -> &str {
    record
        .managed_cwd
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(record.cwd.as_str())
}

pub fn normalize_cutex_session_managed_cwd_path(path: &str) -> anyhow::Result<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() || trimmed == "-" {
        anyhow::bail!("managed cwd path cannot be empty");
    }
    if trimmed == "~" {
        return user_home_dir_string()
            .ok_or_else(|| anyhow::anyhow!("HOME/USERPROFILE is not set"));
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        let home =
            user_home_dir_string().ok_or_else(|| anyhow::anyhow!("HOME/USERPROFILE is not set"))?;
        return Ok(normalize_session_cwd(
            &Path::new(&home).join(rest).display().to_string(),
        ));
    }
    Ok(normalize_session_cwd(trimmed))
}

/// Spelling-only cleanup of a session / agent cwd. Windows paths
/// (`X:\…` / `X:/…`, or `\\…` UNC / verbatim / device paths) get `\`
/// separators, repeated separators collapsed (a JSON- or shell-escaped
/// `E:\\Projects\\x` becomes `E:\Projects\x`) and no trailing separator
/// except at a drive root; a UNC / verbatim path keeps exactly its two
/// leading backslashes (`\\server\share`, `\\?\C:\x`). Everything else
/// (POSIX paths, relative text) is returned unchanged. Pure string work:
/// no filesystem access, no symlink / junction resolution, `\\?\` prefixes
/// stay as they are.
pub fn normalize_session_cwd(raw: &str) -> String {
    let is_separator = |c: char| c == '\\' || c == '/';
    let mut chars = raw.chars();
    let (prefix, rest) = match (chars.next(), chars.next(), chars.next()) {
        (Some(drive), Some(':'), Some(separator))
            if drive.is_ascii_alphabetic() && is_separator(separator) =>
        {
            (format!("{drive}:"), &raw[2..])
        }
        (Some('\\'), Some('\\'), _) => ("\\".to_string(), raw.trim_start_matches(is_separator)),
        _ => return raw.to_string(),
    };
    let components: Vec<&str> = rest.split(is_separator).filter(|part| !part.is_empty()).collect();
    if components.is_empty() {
        // `E:\\` -> `E:\`; a bare `\\` stays as it is.
        return if prefix.ends_with(':') { format!("{prefix}\\") } else { raw.to_string() };
    }
    let mut normalized = format!("{prefix}\\{}", components.join("\\"));
    // A verbatim drive root (`\\?\E:\`) keeps its root separator.
    if prefix == "\\" && normalized.ends_with(':') {
        normalized.push('\\');
    }
    normalized
}

/// The identity of a session / agent cwd for comparisons: the spelling
/// cleanup of [`normalize_session_cwd`] after dropping a Win32 verbatim
/// prefix (`\\?\C:\x` -> `C:\x`, `\\?\UNC\h\s` -> `\\h\s`). `canonicalize()`
/// on Windows returns the verbatim form while owners, Projects and specs
/// spell the same directory without it; both name one directory. Also the
/// form new records store. No case folding, no filesystem access.
pub fn session_cwd_identity(raw: &str) -> String {
    normalize_session_cwd(&crate::agent_management::strip_verbatim_prefix(raw))
}

/// True when two recorded cwds name the same directory spelling, tolerating
/// a verbatim prefix on either side (records written before r245 may hold
/// `\\?\…` from `canonicalize()`). See [`session_cwd_identity`].
pub fn same_session_cwd(left: &str, right: &str) -> bool {
    left == right || session_cwd_identity(left) == session_cwd_identity(right)
}

/// Another live (not archived) record holding `record`'s formal Agent name.
/// Names are unique among live records only; an archived record keeps its
/// name and may only be restored while no live record holds it.
pub fn live_formal_name_holder<'a>(
    store: &'a CutexSessionStore,
    record: &CutexSessionRecord,
) -> Option<&'a CutexSessionRecord> {
    let name = record.formal_agent_name.as_deref()?;
    store.sessions.values().find(|other| {
        other.cutex_session_id != record.cutex_session_id
            && !other.is_retired()
            && other.formal_agent_name.as_deref() == Some(name)
    })
}

/// `#[serde(deserialize_with)]` for request / spec cwd fields: the value as
/// sent, through [`normalize_session_cwd`].
pub fn deserialize_session_cwd<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = <String as serde::Deserialize>::deserialize(deserializer)?;
    Ok(normalize_session_cwd(&raw))
}

/// [`deserialize_session_cwd`] for optional fields (use with `#[serde(default)]`).
pub fn deserialize_optional_session_cwd<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = <Option<String> as serde::Deserialize>::deserialize(deserializer)?;
    Ok(raw.map(|raw| normalize_session_cwd(&raw)))
}

fn user_home_dir_string() -> Option<String> {
    std::env::var("HOME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("USERPROFILE")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
}

pub fn cutex_session_display_name(record: &CutexSessionRecord) -> String {
    record
        .formal_agent_name
        .clone()
        .or_else(|| record.display_name_hint.clone())
        .or_else(|| record.thread_name.clone())
        .or_else(|| record.codex_session_id.clone())
        .unwrap_or_else(|| record.cutex_session_id.clone())
}

/// Apply a validated `cutex/session/metadata/update` patch. Each of
/// `displayName`, `note`, `avatar`, `avatarMode` is tri-state: absent = unchanged,
/// `null` = clear, value = set. Limits are re-checked here so the record
/// can never hold out-of-contract presentation data.
pub fn apply_session_metadata_patch(
    record: &mut CutexSessionRecord,
    params: &serde_json::Value,
) -> anyhow::Result<()> {
    use serde_json::Value;
    let object = params
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("metadata patch must be an object"))?;
    if object.contains_key("avatarMode") && object.get("expectedRevision").and_then(Value::as_u64).is_none() {
        anyhow::bail!("avatarMode changes require expectedRevision as a non-negative integer");
    }
    let mut next = record.clone();
    if let Some(value) = object.get("displayName") {
        next.display_name_override = match value {
            Value::Null => None,
            Value::String(name)
                if !name.trim().is_empty()
                    && name.trim() == name
                    && name.chars().count() <= 64
                    && !name.chars().any(char::is_control) =>
            {
                Some(name.clone())
            }
            _ => anyhow::bail!("displayName must be a valid string or null"),
        };
    }
    if let Some(value) = object.get("note") {
        next.note = match value {
            Value::Null => None,
            Value::String(note)
                if note.chars().count() <= 280
                    && !note
                        .chars()
                        .any(|c| c.is_control() && c != '\n' && c != '\t') =>
            {
                (!note.is_empty()).then(|| note.clone())
            }
            _ => anyhow::bail!("note must be a valid string or null"),
        };
    }
    // An old/automatic client cannot distinguish a deliberately unpinned
    // inherit avatar from an unassigned one. CAS alone cannot express intent:
    // even a freshly read avatar-only write must not silently pin it.
    if record.avatar_mode == Some(crate::session::model::CutexSessionAvatarMode::Inherit)
        && object.contains_key("avatar") && !object.contains_key("avatarMode")
    {
        anyhow::bail!("avatar_mode_explicit_required: inherited avatars require explicit avatarMode fixed or null to select/reset");
    }
    if let Some(value) = object.get("avatarMode") {
        next.avatar_mode = match value {
            Value::Null => None,
            Value::String(mode) if mode == "fixed" => Some(crate::session::model::CutexSessionAvatarMode::Fixed),
            Value::String(mode) if mode == "inherit" => Some(crate::session::model::CutexSessionAvatarMode::Inherit),
            _ => anyhow::bail!("avatarMode must be fixed, inherit or null"),
        };
    }
    if let Some(value) = object.get("avatar") {
        next.avatar = match value {
            Value::Null => None,
            value => {
                let mut avatar: crate::session::model::CutexSessionAvatar =
                    serde_json::from_value(value.clone())?;
                crate::session::model::validate_session_avatar(&avatar)
                    .map_err(|message| anyhow::anyhow!(message))?;
                if let Some(color) = avatar.color.as_mut() {
                    color.make_ascii_lowercase();
                }
                Some(avatar)
            }
        };
    }
    if next.avatar_mode == Some(crate::session::model::CutexSessionAvatarMode::Inherit) {
        if object.get("avatar").is_some_and(|value| !value.is_null()) {
            anyhow::bail!("avatarMode inherit cannot include a non-null avatar");
        }
        next.avatar = None;
    } else if object.contains_key("avatar") && !object.contains_key("avatarMode") {
        // Legacy explicit selection/reset stays compatible and is now pinned.
        next.avatar_mode = Some(crate::session::model::CutexSessionAvatarMode::Fixed);
    }
    if let Some(value) = object.get("badge") {
        next.badge = match value {
            Value::Null => None,
            value => {
                let badge = serde_json::from_value(value.clone())?;
                crate::session::model::validate_session_badge(&badge).map_err(anyhow::Error::msg)?;
                Some(badge)
            }
        };
    }
    *record = next;
    Ok(())
}

/// Whether a `cutex/session/metadata/update` fenced at `expected` may apply
/// to `record`: at the exact revision, or at a later one whose changes since
/// `expected` touched none of the fields the patch writes (cutex's own
/// writes, e.g. a new Agent's startup records, move the session revision
/// too; r252). Without a known history (`metadata_changes` absent, or
/// `expected` older than it) only the exact revision holds.
pub fn metadata_patch_fence_holds(
    record: &CutexSessionRecord,
    expected: u64,
    params: &serde_json::Value,
) -> bool {
    let current = record.durable_revision();
    if expected == current {
        return true;
    }
    let Some(changes) = record.metadata_changes.as_ref() else {
        return false;
    };
    if expected > current || expected < changes.since {
        return false;
    }
    let Some(object) = params.as_object() else {
        return false;
    };
    [
        ("displayName", changes.display_name),
        ("note", changes.note),
        ("avatar", changes.avatar),
        ("avatarMode", changes.avatar),
        ("badge", changes.badge),
    ]
    .into_iter()
    .filter(|(field, _)| object.contains_key(*field))
    .all(|(_, changed)| changed.is_none_or(|revision| revision <= expected))
}

/// Record, at `record`'s (already bumped) durable revision, which metadata
/// fields differ from `before`. A record without history starts it here.
pub fn note_session_metadata_changes(before: &CutexSessionRecord, record: &mut CutexSessionRecord) {
    let revision = record.durable_revision();
    let display_name = before.display_name_override != record.display_name_override;
    let note = before.note != record.note;
    let avatar = before.avatar != record.avatar || before.avatar_mode != record.avatar_mode;
    let badge = before.badge != record.badge;
    let changes = record
        .metadata_changes
        .get_or_insert_with(|| crate::session::model::CutexSessionMetadataChanges::since(revision));
    for (changed, slot) in [
        (display_name, &mut changes.display_name),
        (note, &mut changes.note),
        (avatar, &mut changes.avatar),
        (badge, &mut changes.badge),
    ] {
        if changed {
            *slot = Some(revision);
        }
    }
}

/// Client-facing label: a Human `display_name_override` wins, then the
/// identity-oriented [`cutex_session_display_name`] chain
/// (formal_agent_name > display_name_hint > thread_name > ids). Use only
/// for presentation; launch naming, bus addressing and authority keep
/// using `formal_agent_name` / [`cutex_session_display_name`].
pub fn cutex_session_presented_name(record: &CutexSessionRecord) -> String {
    record
        .display_name_override
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| cutex_session_display_name(record))
}

pub fn cutex_session_is_managed(record: &CutexSessionRecord) -> bool {
    record.registration_class == AgentRegistrationClass::Persistent
}

pub fn cutex_session_key_for_codex_session(
    store: &CutexSessionStore,
    codex_session_id: &str,
) -> String {
    store
        .sessions
        .iter()
        .find_map(|(key, record)| {
            (record.is_active() && record.codex_session_id.as_deref() == Some(codex_session_id))
                .then(|| key.clone())
        })
        .unwrap_or_else(|| default_cutex_session_id_for_codex_session(codex_session_id))
}

pub fn cutex_session_key_for_user_id(store: &CutexSessionStore, id: &str) -> Option<String> {
    cutex_session_key_for_user_id_matching(store, id, CutexSessionRecord::is_active)
}

pub fn cutex_session_key_for_user_id_including_retired(
    store: &CutexSessionStore,
    id: &str,
) -> Option<String> {
    cutex_session_key_for_user_id_matching(store, id, |_| true)
}

pub fn cutex_session_key_for_codex_session_including_retired(
    store: &CutexSessionStore,
    codex_session_id: &str,
) -> Option<String> {
    store.sessions.iter().find_map(|(key, record)| {
        (record.codex_session_id.as_deref() == Some(codex_session_id)).then(|| key.clone())
    })
}

fn cutex_session_key_for_user_id_matching(
    store: &CutexSessionStore,
    id: &str,
    include: impl Fn(&CutexSessionRecord) -> bool + Copy,
) -> Option<String> {
    let id = id.trim();
    if id.is_empty() {
        return None;
    }
    if store.sessions.get(id).is_some_and(include) {
        return Some(id.to_string());
    }
    store
        .sessions
        .iter()
        .find_map(|(key, record)| {
            (include(record) && record.codex_session_id.as_deref() == Some(id)).then(|| key.clone())
        })
        .or_else(|| unique_cutex_session_key_by_name(store, id, include))
}

fn unique_cutex_session_key_by_name(
    store: &CutexSessionStore,
    name: &str,
    include: impl Fn(&CutexSessionRecord) -> bool + Copy,
) -> Option<String> {
    let needle = name.trim();
    if needle.is_empty() {
        return None;
    }
    let mut matches = store.sessions.iter().filter_map(|(key, record)| {
        (include(record)
            && [
                record.display_name_hint.as_deref(),
                record.thread_name.as_deref(),
            ]
            .into_iter()
            .flatten()
            .any(|candidate| candidate == needle))
        .then(|| key.clone())
    });
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badge_validation_roundtrip_and_field_fence() {
        let mut r = CutexSessionRecord::from_codex_session_id("018a0000-0000-7000-8000-00000000b003").unwrap();
        let revision = r.durable_revision();
        for text in ["ABCD", "中文", "A", "🦊"] {
            apply_session_metadata_patch(&mut r, &serde_json::json!({"badge": {"text": text, "tint": "blue"}})).unwrap();
            let loaded: CutexSessionRecord = serde_json::from_value(serde_json::to_value(&r).unwrap()).unwrap();
            assert_eq!(loaded.badge, r.badge);
        }
        let before = r.clone();
        for badge in [serde_json::json!({"text": ""}), serde_json::json!({"text": "ABCDE"}), serde_json::json!({"text": "中文人"}), serde_json::json!({"text": "a b"}), serde_json::json!({"text": "a\n"}), serde_json::json!({"text": "A", "tint": "red"}), serde_json::json!({"text": "A", "extra": true})] {
            assert!(apply_session_metadata_patch(&mut r, &serde_json::json!({"badge": badge})).is_err());
            assert_eq!(before, r);
        }
        apply_session_metadata_patch(&mut r, &serde_json::json!({"badge": {"text": "NEW"}})).unwrap();
        r.bump_durable_revision().unwrap();
        note_session_metadata_changes(&before, &mut r);
        assert!(!metadata_patch_fence_holds(&r, revision, &serde_json::json!({"badge": null})));
        assert!(metadata_patch_fence_holds(&r, revision, &serde_json::json!({"note": "ok"})));
        apply_session_metadata_patch(&mut r, &serde_json::json!({"note": "ok"})).unwrap();
        assert_eq!(r.badge.as_ref().unwrap().text, "NEW");
        apply_session_metadata_patch(&mut r, &serde_json::json!({"badge": null})).unwrap();
        assert!(r.badge.is_none());
    }

    #[test]
    fn session_cwd_double_escaped_windows_path_collapses() {
        assert_eq!(
            normalize_session_cwd(r"E:\\Projects (Orchid)\\waveline-backend"),
            r"E:\Projects (Orchid)\waveline-backend"
        );
        assert_eq!(normalize_session_cwd(r"E:\\\\a\\\\b"), r"E:\a\b");
        // Already clean: unchanged.
        let clean = r"E:\Projects (Orchid)\waveline-backend";
        assert_eq!(normalize_session_cwd(clean), clean);
    }

    #[test]
    fn session_cwd_mixed_separators_become_backslashes() {
        assert_eq!(normalize_session_cwd(r"E:/Projects\\x/y"), r"E:\Projects\x\y");
        assert_eq!(normalize_session_cwd("c:/Users/pat"), r"c:\Users\pat");
    }

    #[test]
    fn session_cwd_unc_and_verbatim_prefixes_stay_valid() {
        assert_eq!(normalize_session_cwd(r"\\server\share\dir"), r"\\server\share\dir");
        // Double-escaped UNC keeps exactly two leading backslashes.
        assert_eq!(normalize_session_cwd(r"\\\\server\\share\\dir"), r"\\server\share\dir");
        assert_eq!(normalize_session_cwd(r"\\server\share\\"), r"\\server\share");
        assert_eq!(normalize_session_cwd(r"\\?\E:\Projects\x"), r"\\?\E:\Projects\x");
        assert_eq!(normalize_session_cwd(r"\\?\E:\\Projects\\x"), r"\\?\E:\Projects\x");
        assert_eq!(normalize_session_cwd(r"\\?\E:\"), r"\\?\E:\");
        assert_eq!(normalize_session_cwd(r"\\?\UNC\server\share"), r"\\?\UNC\server\share");
        assert_eq!(normalize_session_cwd(r"\\"), r"\\");
    }

    #[test]
    fn session_cwd_trailing_separator_is_dropped_except_at_a_drive_root() {
        assert_eq!(normalize_session_cwd(r"E:\Projects\x\"), r"E:\Projects\x");
        assert_eq!(normalize_session_cwd(r"E:\Projects\x\\"), r"E:\Projects\x");
        assert_eq!(normalize_session_cwd("E:/x/"), r"E:\x");
        assert_eq!(normalize_session_cwd(r"E:\"), r"E:\");
        assert_eq!(normalize_session_cwd(r"E:\\"), r"E:\");
        assert_eq!(normalize_session_cwd("E:/"), r"E:\");
    }

    #[test]
    fn session_cwd_posix_and_other_text_is_untouched() {
        for raw in [
            "/tmp",
            "/home/u//projects/",
            "//network/like",
            "/a\\b",
            "relative/dir",
            "E:",
            "E:relative",
            "",
            " E:\\x",
        ] {
            assert_eq!(normalize_session_cwd(raw), raw, "{raw:?}");
        }
    }

    #[test]
    fn session_cwd_request_fields_normalize_on_deserialize() {
        #[derive(serde::Deserialize)]
        struct Request {
            #[serde(deserialize_with = "deserialize_session_cwd")]
            cwd: String,
            #[serde(default, deserialize_with = "deserialize_optional_session_cwd")]
            other: Option<String>,
        }
        let request: Request =
            serde_json::from_str(r#"{"cwd":"E:\\\\Projects\\\\x","other":"D:/a/"}"#).unwrap();
        assert_eq!(request.cwd, r"E:\Projects\x");
        assert_eq!(request.other.as_deref(), Some(r"D:\a"));
        let request: Request = serde_json::from_str(r#"{"cwd":"/tmp/x"}"#).unwrap();
        assert_eq!((request.cwd.as_str(), request.other), ("/tmp/x", None));
    }

    #[test]
    fn verbatim_and_plain_windows_cwds_are_the_same_identity() {
        // r244: a Claude agent created in `F:\cutex-test\r244` stored
        // `\\?\F:\cutex-test\r244` (canonicalize) and every managed check
        // compared the two texts.
        assert_eq!(session_cwd_identity(r"\\?\F:\cutex-test\r244"), r"F:\cutex-test\r244");
        assert_eq!(session_cwd_identity(r"\\?\F:\cutex-test\r244\"), r"F:\cutex-test\r244");
        assert_eq!(session_cwd_identity(r"\\?\UNC\host\share\x"), r"\\host\share\x");
        assert_eq!(session_cwd_identity(r"\\?\E:\"), r"E:\");
        assert!(same_session_cwd(r"\\?\F:\cutex-test\r244", r"F:\cutex-test\r244"));
        assert!(same_session_cwd(r"F:\cutex-test\r244", r"F:/cutex-test/r244/"));
        assert!(!same_session_cwd(r"\\?\F:\cutex-test\r244", r"F:\cutex-test\r245"));
        // Case is not folded and POSIX paths are compared as written.
        assert!(!same_session_cwd(r"F:\X", r"f:\x"));
        assert!(same_session_cwd("/srv/x", "/srv/x"));
        assert!(!same_session_cwd("/srv/x", "/srv/y"));
    }

    #[test]
    fn session_record_and_managed_cwd_inputs_are_normalized() {
        let record = CutexSessionRecord::new_at(
            "cutex.cwd-input".to_string(),
            None,
            "host".to_string(),
            r"E:\\Projects\\x".to_string(),
            None,
            "2026-10-11T00:00:00Z".to_string(),
        )
        .unwrap();
        assert_eq!(record.cwd, r"E:\Projects\x");
        assert_eq!(
            normalize_cutex_session_managed_cwd_path(r"  E:\\Projects\\y\  ").unwrap(),
            r"E:\Projects\y"
        );
        assert_eq!(normalize_cutex_session_managed_cwd_path("/srv/x").unwrap(), "/srv/x");
    }

    #[test]
    fn metadata_fence_is_per_field_once_the_history_is_known() {
        let mut record = CutexSessionRecord::new_at(
            "cutex.fence".to_string(),
            None,
            "host".to_string(),
            "/srv/x".to_string(),
            None,
            "2026-10-23T00:00:00Z".to_string(),
        )
        .unwrap();
        let created = record.durable_revision();
        // An internal write (not metadata) moves the revision.
        record.agent_enabled = true;
        record.bump_durable_revision().unwrap();
        let avatar = serde_json::json!({ "avatar": { "kind": "emoji", "value": "F" } });
        assert!(metadata_patch_fence_holds(&record, created, &avatar));
        assert!(!metadata_patch_fence_holds(&record, created + 5, &avatar), "future");
        // A metadata write changing the avatar (same value again: no change).
        let before = record.clone();
        apply_session_metadata_patch(&mut record, &avatar).unwrap();
        record.bump_durable_revision().unwrap();
        note_session_metadata_changes(&before, &mut record);
        let at = record.durable_revision();
        assert_eq!(record.metadata_changes.as_ref().unwrap().avatar, Some(at));
        assert_eq!(record.metadata_changes.as_ref().unwrap().note, None);
        assert!(!metadata_patch_fence_holds(&record, created, &avatar));
        assert!(metadata_patch_fence_holds(&record, at - 1, &serde_json::json!({ "note": "n" })));
        assert!(metadata_patch_fence_holds(&record, at, &avatar));
        let before = record.clone();
        apply_session_metadata_patch(&mut record, &avatar).unwrap();
        record.bump_durable_revision().unwrap();
        note_session_metadata_changes(&before, &mut record);
        assert_eq!(record.metadata_changes.as_ref().unwrap().avatar, Some(at), "unchanged value");
        // No history (a record saved by an older cutex): exact match only.
        record.metadata_changes = None;
        assert!(!metadata_patch_fence_holds(&record, at, &serde_json::json!({ "note": "n" })));
        assert!(metadata_patch_fence_holds(
            &record,
            record.durable_revision(),
            &serde_json::json!({ "note": "n" })
        ));
        // Older records parse without the field and are saved without it.
        let mut value = serde_json::to_value(&record).unwrap();
        assert!(value.get("metadata_changes").is_none());
        value["metadata_changes"] = serde_json::json!({ "since": 3, "avatar": 4 });
        let parsed: CutexSessionRecord = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.metadata_changes.unwrap().avatar, Some(4));
    }

    #[test]
    fn metadata_patch_is_tri_state_and_override_wins_only_for_presentation() {
        let mut record =
            CutexSessionRecord::from_codex_session_id("018a0000-0000-7000-8000-00000000b001")
                .expect("record");
        record.formal_agent_name = Some("Worker".to_string());
        assert_eq!(cutex_session_presented_name(&record), "Worker");

        apply_session_metadata_patch(
            &mut record,
            &serde_json::json!({
                "expectedRevision": 1,
                "displayName": "小助手",
                "note": "line one\nline two",
                "avatar": { "kind": "emoji", "value": "🦊" }
            }),
        )
        .expect("set metadata");
        assert_eq!(cutex_session_presented_name(&record), "小助手");
        assert_eq!(cutex_session_display_name(&record), "Worker");
        assert_eq!(record.formal_agent_name.as_deref(), Some("Worker"));
        assert_eq!(record.note.as_deref(), Some("line one\nline two"));
        assert_eq!(
            record.avatar,
            Some(crate::session::model::CutexSessionAvatar {
                kind: crate::session::model::CutexSessionAvatarKind::Emoji,
                value: "🦊".to_string(),
                color: None,
            })
        );

        // Omitted fields stay; null clears.
        apply_session_metadata_patch(&mut record, &serde_json::json!({ "displayName": null }))
            .expect("clear name");
        assert_eq!(cutex_session_presented_name(&record), "Worker");
        assert!(record.note.is_some() && record.avatar.is_some());

        let before = record.clone();
        for invalid in [
            serde_json::json!({ "displayName": "" }),
            serde_json::json!({ "displayName": " padded " }),
            serde_json::json!({ "displayName": "x".repeat(65) }),
            serde_json::json!({ "note": "n".repeat(281) }),
            serde_json::json!({ "avatar": { "kind": "image", "value": "x" } }),
            serde_json::json!({ "avatar": { "kind": "color", "value": "c".repeat(65) } }),
            serde_json::json!({ "avatar": { "kind": "emoji", "value": "🦊", "color": "red" } }),
            serde_json::json!({ "avatar": { "kind": "emoji", "value": "🦊", "color": "#e8590" } }),
            serde_json::json!({ "avatar": { "kind": "color", "value": "#e8590c", "color": "#e8590c" } }),
        ] {
            assert!(apply_session_metadata_patch(&mut record, &invalid).is_err(), "{invalid}");
        }
        assert_eq!(record, before, "a rejected patch must not partially apply");

        // Old stores without the new fields still load.
        let mut legacy = serde_json::to_value(&before).expect("serialize");
        let legacy_object = legacy.as_object_mut().unwrap();
        legacy_object.remove("display_name_override");
        legacy_object.remove("note");
        legacy_object.remove("avatar");
        let loaded: CutexSessionRecord = serde_json::from_value(legacy).expect("legacy record");
        assert!(loaded.display_name_override.is_none() && loaded.avatar.is_none());
    }

    #[test]
    fn avatar_color_combines_with_emoji_and_round_trips_compatibly() {
        use crate::session::model::{CutexSessionAvatar, CutexSessionAvatarKind};
        let mut record =
            CutexSessionRecord::from_codex_session_id("018a0000-0000-7000-8000-00000000b002")
                .expect("record");
        apply_session_metadata_patch(
            &mut record,
            &serde_json::json!({ "avatar": { "kind": "emoji", "value": "🦊", "color": "#E8590C" } }),
        )
        .expect("emoji + colour");
        assert_eq!(
            record.avatar,
            Some(CutexSessionAvatar {
                kind: CutexSessionAvatarKind::Emoji,
                value: "🦊".into(),
                color: Some("#e8590c".into()),
            })
        );
        let stored = serde_json::to_value(&record).expect("serialize");
        assert_eq!(
            stored["avatar"],
            serde_json::json!({ "kind": "emoji", "value": "🦊", "color": "#e8590c" })
        );

        // The old shape is still written without `color`.
        apply_session_metadata_patch(
            &mut record,
            &serde_json::json!({ "avatar": { "kind": "color", "value": "#40c057" } }),
        )
        .expect("colour kind");
        assert_eq!(
            serde_json::to_value(&record).unwrap()["avatar"],
            serde_json::json!({ "kind": "color", "value": "#40c057" })
        );

        // A pre-colour reader (same struct minus `color`, no deny_unknown_fields)
        // reads a new record and simply drops the colour.
        #[derive(serde::Deserialize, serde::Serialize)]
        struct OldAvatar {
            kind: CutexSessionAvatarKind,
            value: String,
        }
        let old: OldAvatar = serde_json::from_value(
            serde_json::json!({ "kind": "emoji", "value": "🦊", "color": "#e8590c" }),
        )
        .expect("old reader ignores color");
        assert_eq!(
            serde_json::to_value(&old).unwrap(),
            serde_json::json!({ "kind": "emoji", "value": "🦊" })
        );
    }

    #[test]
    fn managed_cwd_path_normalization_rejects_empty_values() {
        assert!(normalize_cutex_session_managed_cwd_path("").is_err());
        assert!(normalize_cutex_session_managed_cwd_path("   ").is_err());
        assert!(normalize_cutex_session_managed_cwd_path("-").is_err());
    }

    #[test]
    fn managed_cwd_path_normalization_trims_literal_path() {
        let normalized =
            normalize_cutex_session_managed_cwd_path("  /tmp/cutex-session  ").expect("path");
        assert_eq!(normalized, "/tmp/cutex-session");
    }
}

#[cfg(test)]
mod avatar_mode_tests {
    use super::*;
    use crate::session::model::CutexSessionAvatarMode as Mode;
    use serde_json::{json, Value};

    fn record() -> CutexSessionRecord {
        CutexSessionRecord::from_codex_session_id("018a0000-0000-7000-8000-00000000b258").unwrap()
    }
    fn avatar() -> Value { json!({"kind":"emoji","value":"🦊","color":"#E8590C"}) }
    fn mode_patch(record: &CutexSessionRecord, mode: Value) -> Value {
        json!({"expectedRevision":record.durable_revision(),"avatarMode":mode})
    }
    fn commit(record: &mut CutexSessionRecord, patch: &Value) {
        assert!(metadata_patch_fence_holds(record, patch["expectedRevision"].as_u64().unwrap(), patch));
        let before=record.clone();
        apply_session_metadata_patch(record,patch).unwrap();
        record.bump_durable_revision().unwrap();
        note_session_metadata_changes(&before,record);
    }

    #[test]
    fn legacy_missing_null_mode_and_avatar_selection_keep_fixed_semantics() {
        let mut r=record();assert_eq!(r.avatar_mode,None);
        let original=serde_json::to_value(&r).unwrap();assert!(original.get("avatar_mode").is_none());
        let mut null=original.clone();null["avatar_mode"]=Value::Null;
        assert_eq!(serde_json::from_value::<CutexSessionRecord>(null).unwrap().avatar_mode,None);
        apply_session_metadata_patch(&mut r,&json!({"avatar":avatar()})).unwrap();
        assert_eq!(r.avatar_mode,Some(Mode::Fixed));assert_eq!(r.avatar.as_ref().unwrap().color.as_deref(),Some("#e8590c"));
        apply_session_metadata_patch(&mut r,&json!({"avatar":null})).unwrap();
        assert!(r.avatar.is_none());assert_eq!(r.avatar_mode,Some(Mode::Fixed));
    }

    #[test]
    fn inherit_clears_stored_avatar_and_refuses_automatic_avatar_only_even_at_current_revision() {
        let mut r=record();apply_session_metadata_patch(&mut r,&json!({"avatar":avatar()})).unwrap();
        let patch=mode_patch(&r,json!("inherit"));commit(&mut r,&patch);
        assert_eq!(r.avatar_mode,Some(Mode::Inherit));assert!(r.avatar.is_none());
        let before=serde_json::to_value(&r).unwrap();
        for value in [avatar(),Value::Null] {
            let patch=json!({"expectedRevision":r.durable_revision(),"avatar":value,"note":"must not partly apply"});
            assert!(metadata_patch_fence_holds(&r,r.durable_revision(),&patch));
            assert!(apply_session_metadata_patch(&mut r,&patch).unwrap_err().to_string().contains("avatar_mode_explicit_required"));
            assert_eq!(serde_json::to_value(&r).unwrap(),before);
        }
    }

    #[test]
    fn explicit_fixed_or_legacy_reset_is_atomic_and_inherit_conflicts_are_rejected() {
        for fixed in [json!("fixed"),Value::Null] {
            for selected in [avatar(),Value::Null] {
                let mut r=record();let patch=mode_patch(&r,json!("inherit"));commit(&mut r,&patch);
                let patch=json!({"expectedRevision":r.durable_revision(),"avatarMode":fixed,"avatar":selected});
                commit(&mut r,&patch);
                assert_eq!(r.avatar_mode,if fixed.is_null(){None}else{Some(Mode::Fixed)});
                assert_eq!(r.avatar.is_some(),!selected.is_null());
            }
        }
        let mut r=record();let before=serde_json::to_value(&r).unwrap();
        for bad in [json!({"avatarMode":"inherit","avatar":avatar()}),json!({"avatarMode":"dynamic"}),json!({"avatarMode":true}),json!({"avatarMode":{"kind":"inherit"}})] {
            let mut patch=bad;patch["expectedRevision"]=json!(r.durable_revision());patch["displayName"]=json!("must not partly apply");
            assert!(apply_session_metadata_patch(&mut r,&patch).is_err());assert_eq!(serde_json::to_value(&r).unwrap(),before);
        }
    }

    #[test]
    fn mode_changes_require_revision_but_legacy_metadata_remains_compatible() {
        let mut r=record();let before=serde_json::to_value(&r).unwrap();
        for mode in [json!("fixed"),json!("inherit"),Value::Null] {
            for revision in [None,Some(Value::Null),Some(json!(-1)),Some(json!("1")),Some(json!(1.5))] {
                let mut patch=json!({"avatarMode":mode});
                if let Some(revision)=revision {patch["expectedRevision"]=revision;}
                assert!(apply_session_metadata_patch(&mut r,&patch).is_err());assert_eq!(serde_json::to_value(&r).unwrap(),before);
            }
        }
        apply_session_metadata_patch(&mut r,&json!({"note":"legacy un-fenced note"})).unwrap();
        assert_eq!(r.note.as_deref(),Some("legacy un-fenced note"));
    }

    #[test]
    fn avatar_and_mode_share_one_cas_domain_in_both_directions() {
        let mut r=record();let old=r.durable_revision();
        let patch=mode_patch(&r,json!("inherit"));commit(&mut r,&patch);
        assert!(!metadata_patch_fence_holds(&r,old,&json!({"avatar":null})),"mode-only change fences an old first-seen avatar write");
        assert!(!metadata_patch_fence_holds(&r,old,&json!({"avatarMode":"fixed"})));
        assert!(metadata_patch_fence_holds(&r,old,&json!({"note":"unrelated"})));
        let current=r.durable_revision();
        r.bump_durable_revision().unwrap();
        assert!(metadata_patch_fence_holds(&r,current,&json!({"expectedRevision":current,"avatarMode":"fixed"})),"unrelated internal writes preserve field-level CAS");
        let patch=json!({"expectedRevision":r.durable_revision(),"avatarMode":"fixed","avatar":avatar()});commit(&mut r,&patch);
        assert!(!metadata_patch_fence_holds(&r,current,&json!({"avatarMode":"inherit"})),"avatar write also fences mode changes");
        assert_eq!(r.metadata_changes.as_ref().unwrap().avatar,Some(r.durable_revision()));
    }

    #[test]
    fn mode_and_fixed_avatar_survive_session_store_persistence_and_restore() {
        let root=std::env::temp_dir().join(format!("session-avatar-mode-{}",uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();let path=root.join("cutex-sessions.json");
        let mut store=crate::session::store::load_cutex_session_store_from_path(&path).unwrap();
        let mut fixed=record();apply_session_metadata_patch(&mut fixed,&json!({"avatar":avatar()})).unwrap();
        let mut inherit=CutexSessionRecord::from_codex_session_id("018a0000-0000-7000-8000-00000000b259").unwrap();
        let patch=mode_patch(&inherit,json!("inherit"));commit(&mut inherit,&patch);
        store.sessions.insert(fixed.cutex_session_id.clone(),fixed.clone());store.sessions.insert(inherit.cutex_session_id.clone(),inherit.clone());
        crate::session::store::save_cutex_session_store_to_path(&path,&store).unwrap();
        let restored=crate::session::store::load_cutex_session_store_from_path(&path).unwrap();
        assert_eq!(restored.sessions[&fixed.cutex_session_id].avatar,fixed.avatar);
        assert_eq!(restored.sessions[&fixed.cutex_session_id].avatar_mode,Some(Mode::Fixed));
        assert_eq!(restored.sessions[&inherit.cutex_session_id].avatar_mode,Some(Mode::Inherit));
        assert!(restored.sessions[&inherit.cutex_session_id].avatar.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}
