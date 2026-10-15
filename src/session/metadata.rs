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
/// `displayName`, `note`, `avatar` is tri-state: absent = unchanged,
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
    *record = next;
    Ok(())
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
