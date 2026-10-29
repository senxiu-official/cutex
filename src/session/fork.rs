//! Durable fork provenance, kept outside old session-record schemas.
use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkOrigin {
    pub source_session_id: String,
    pub source_profile: Option<String>,
    pub target_profile: Option<String>,
    pub history_bytes: u64,
    pub cold_cache: bool,
}
impl ForkOrigin {
    pub fn boundary_note(&self) -> String {
        format!("This is a NEW fork of session {}. The copied conversation is reference history. Delegations, asks, jobs, workflow runs, approvals and other obligations mentioned in that history belong to the SOURCE session, not this agent. Do not resume, answer or claim them as your own; use your new session identity for new work.",self.source_session_id)
    }
}
fn path(root: &Path, id: &str) -> anyhow::Result<PathBuf> {
    let uuid = id
        .strip_prefix("cutex.")
        .context("fork requires a cutex session id")?;
    let uuid = uuid::Uuid::parse_str(uuid)?;
    Ok(root.join(format!("{uuid}.json")))
}
pub fn save_at(root: &Path, id: &str, origin: &ForkOrigin) -> anyhow::Result<()> {
    ensure!(
        !origin.source_session_id.chars().any(char::is_control),
        "invalid source identity"
    );
    crate::config::atomic::write_private_pretty_json_atomic(
        &path(root, id)?,
        origin,
        "session fork provenance",
    )
}
pub fn save(id: &str, origin: &ForkOrigin) -> anyhow::Result<()> {
    save_at(
        &crate::config::paths::runtime_dir()?.join("session-forks"),
        id,
        origin,
    )
}
pub fn load(id: &str) -> Option<ForkOrigin> {
    let path = path(
        &crate::config::paths::runtime_dir()
            .ok()?
            .join("session-forks"),
        id,
    )
    .ok()?;
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 64 * 1024 {
        return None;
    }
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}
pub fn boundary_note(id: &str) -> Option<String> {
    load(id).map(|origin| origin.boundary_note())
}
/// Preserve source presentation intent on a NEW fork, independent of its target
/// Profile. A fixed source keeps its exact avatar; explicit inherit follows the
/// fork's target Profile/app and never pins a stale source avatar. This helper
/// is for creation before persistence/publication, not an unfenced live edit.
pub fn inherit_fork_avatar(source: &crate::session::model::CutexSessionRecord, target: &mut crate::session::model::CutexSessionRecord) {
    target.avatar_mode = source.avatar_mode;
    target.avatar = if source.avatar_mode == Some(crate::session::model::CutexSessionAvatarMode::Inherit) {
        None
    } else {
        source.avatar.clone()
    };
}

/// Account-specific encrypted reasoning / thinking signatures must never be silently stripped.
pub fn signature_rejection(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    (text.contains("encrypted")
        && (text.contains("reasoning") || text.contains("decrypt") || text.contains("content")))
        || ((text.contains("thinking") || text.contains("reasoning")) && text.contains("signature"))
}
pub fn rejection_message(id: &str, text: &str) -> Option<String> {
    let origin = load(id)?;
    rejection_message_for(&origin, text)
}
fn rejection_message_for(origin: &ForkOrigin, text: &str) -> Option<String> {
    (origin.cold_cache && signature_rejection(text) && !text.starts_with("The target account rejected")).then(|| format!("The target account rejected encrypted reasoning or a thinking signature in this fork's copied history. The fork and source are preserved; no history was stripped or deleted. Source session: {}. Original error: {text}",origin.source_session_id))
}
pub fn annotate_native_rejection(id: &str, message: &mut serde_json::Value) {
    for pointer in ["/params/turn/error/message", "/params/error/message"] {
        let Some(text) = message.pointer(pointer).and_then(serde_json::Value::as_str) else {
            continue;
        };
        if !signature_rejection(text) {
            continue;
        }
        if let Some(note) = rejection_message(id, text) {
            if let Some(value) = message.pointer_mut(pointer) {
                *value = serde_json::Value::String(note);
            }
        }
    }
}

pub fn codex_history_bytes(home: &Path, thread: &str) -> anyhow::Result<u64> {
    let current = crate::launch::native_history::current(home, thread)?;
    crate::launch::native_history::recovery_sources(home, &current)?
        .iter()
        .try_fold(0u64, |sum, path| {
            sum.checked_add(std::fs::metadata(path)?.len())
                .context("fork history size overflow")
        })
}

/// A presentation-only size failure cannot reject a completed native fork.
/// The old numeric provenance field uses 0 when unknown; the public result
/// separately returns null and an explicit warning.
pub fn optional_history_bytes(result: anyhow::Result<u64>) -> (Option<u64>, Option<serde_json::Value>) {
    match result {
        Ok(bytes) => (Some(bytes), None),
        Err(error) => (None, Some(serde_json::json!({"code":"fork_history_size_unavailable","message":format!("Fork history size is unavailable: {error:#}. The fork and its history are preserved.")}))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn provenance_is_separate_and_boundary_names_source_obligations() {
        let root = std::env::temp_dir().join(format!("fork-origin-{}", uuid::Uuid::new_v4()));
        let id = format!("cutex.{}", uuid::Uuid::new_v4());
        let origin = ForkOrigin {
            source_session_id: "cutex.source".into(),
            source_profile: Some("a".into()),
            target_profile: Some("b".into()),
            history_bytes: 42,
            cold_cache: true,
        };
        save_at(&root, &id, &origin).unwrap();
        let loaded: ForkOrigin =
            serde_json::from_slice(&std::fs::read(path(&root, &id).unwrap()).unwrap()).unwrap();
        assert_eq!(loaded, origin);
        assert!(loaded.boundary_note().contains("SOURCE session"));
        assert!(path(&root, "../../auth").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn recognizes_signature_rejections_without_rewriting_history() {
        let origin = ForkOrigin {
            source_session_id: "cutex.source".into(),
            source_profile: None,
            target_profile: Some("target".into()),
            history_bytes: 1024,
            cold_cache: true,
        };
        let message = rejection_message_for(&origin, "Invalid thinking block signature").unwrap();
        assert!(message.contains("fork and source are preserved"));
        assert!(message.contains("cutex.source"));
        assert!(rejection_message_for(&origin, &message).is_none());
        assert!(signature_rejection("Invalid thinking block signature"));
        assert!(signature_rejection(
            "Encrypted reasoning content could not be decrypted"
        ));
        assert!(!signature_rejection("rate limit reached"));
        assert!(!signature_rejection("tool call signature mismatch"));
    }
}

#[cfg(test)]
mod avatar_mode_fork_tests {
    use super::*;
    use crate::session::model::{CutexSessionRecord, CutexSessionAvatar, CutexSessionAvatarKind, CutexSessionAvatarMode};

    #[test]
    fn same_and_cross_profile_forks_preserve_fixed_avatar_and_legacy_mode() {
        let mut source=CutexSessionRecord::from_codex_session_id("018a0000-0000-7000-8000-00000000b258").unwrap();
        source.profile=Some("source-profile".into());
        source.avatar=Some(CutexSessionAvatar {kind:CutexSessionAvatarKind::Icon,value:"pack/theme".into(),color:Some("#123456".into())});
        for mode in [None,Some(CutexSessionAvatarMode::Fixed)] {
            source.avatar_mode=mode;
            for profile in [Some("source-profile"),Some("different-profile"),None] {
                let mut target=CutexSessionRecord::from_codex_session_id("018a0000-0000-7000-8000-00000000b259").unwrap();
                target.profile=profile.map(str::to_owned);inherit_fork_avatar(&source,&mut target);
                assert_eq!(target.avatar,source.avatar);assert_eq!(target.avatar_mode,mode);
                assert_eq!(target.profile.as_deref(),profile,"avatar copying never changes the fork's actual profile");
            }
        }
    }

    #[test]
    fn explicit_inherit_follows_the_target_profile_without_pinning_a_source_avatar() {
        let mut source=CutexSessionRecord::from_codex_session_id("018a0000-0000-7000-8000-00000000b258").unwrap();
        source.profile=Some("source-profile".into());source.avatar_mode=Some(CutexSessionAvatarMode::Inherit);
        // Even a restored/inconsistent cached source avatar cannot become fixed.
        source.avatar=Some(CutexSessionAvatar{kind:CutexSessionAvatarKind::Emoji,value:"🦊".into(),color:None});
        for profile in [Some("source-profile"),Some("different-profile"),None] {
            let mut target=source.clone();target.profile=profile.map(str::to_owned);
            inherit_fork_avatar(&source,&mut target);
            assert_eq!(target.avatar_mode,Some(CutexSessionAvatarMode::Inherit));assert!(target.avatar.is_none());
            assert_eq!(target.profile.as_deref(),profile);
        }
    }
}
