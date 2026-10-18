//! Rollback swap step (services stopped): rewrite the few records an older
//! cutex cannot parse at all, so it loads the rest of its stores. Every
//! record changed here is copied to `displaced/older-stores/` first (never
//! deleted); a roll-forward does not need them back.
//!
//! - Approval records of kind `static_root` (cutex 0.1.4): an older release
//!   rejects the whole `runtime/approvals/approvals.json` ("approval store is
//!   corrupt"). They are removed; a pending one is simply asked again.
//! - `ClaudeSession.initial_permission_mode` (0.1.4, pending until the
//!   agent's first successful start): `ClaudeSession` is
//!   `deny_unknown_fields`, so an older release cannot load any session. The
//!   field is cleared; the agent then starts in the Profile's default mode,
//!   as it would have on that release.

use std::path::Path;

use serde_json::json;

const APPROVALS: &str = "runtime/approvals/approvals.json";
const SESSIONS: &str = "cutex-sessions.json";
const OUT_DIR: &str = "older-stores";

pub fn prepare(home: &Path, displaced: &Path) -> anyhow::Result<Vec<String>> {
    let mut notes = Vec::new();
    let approvals = home.join(APPROVALS);
    if approvals.is_file() {
        let removed = crate::approvals::ApprovalStore::at(&approvals)
            .remove_kind(crate::approvals::ApprovalKind::StaticRoot)?;
        if !removed.is_empty() {
            save(displaced, "approvals-static-root.json", &json!({ "approvals": removed }))?;
            notes.push(format!("{APPROVALS}: {} static_root approval record(s) removed", removed.len()));
        }
    }
    let sessions = home.join(SESSIONS);
    if crate::session::store::session_database_path(&sessions).is_file() {
        let cleared = crate::session::store::with_locked_session_rows(&sessions, |rows| {
            let keys: Vec<String> = rows
                .sessions()
                .filter(|(_, record)| {
                    record.claude_session.as_ref().is_some_and(|claude| claude.initial_permission_mode.is_some())
                })
                .map(|(key, _)| key.clone())
                .collect();
            let mut cleared = Vec::new();
            for key in keys {
                let record = rows.session_mut(&key).expect("key listed above");
                let mode = record.claude_session.as_mut().and_then(|claude| claude.initial_permission_mode.take());
                record.bump_durable_revision()?;
                record.updated_at = chrono::Utc::now().to_rfc3339();
                cleared.push(json!({ "key": key, "name": record.formal_agent_name, "initialPermissionMode": mode }));
            }
            if !cleared.is_empty() {
                rows.commit()?;
            }
            Ok(cleared)
        })?;
        if !cleared.is_empty() {
            save(displaced, "claude-initial-permission-modes.json", &json!({ "sessions": cleared }))?;
            notes.push(format!("{SESSIONS}: pending Claude permission mode cleared on {} agent(s)", cleared.len()));
        }
    }
    Ok(notes)
}

fn save(displaced: &Path, name: &str, value: &serde_json::Value) -> anyhow::Result<()> {
    let path = displaced.join(OUT_DIR).join(name);
    anyhow::ensure!(!path.exists(), "{} already exists", path.display());
    crate::config::atomic::write_private_pretty_json_atomic(&path, value, "rollback record")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approvals::{ApprovalKind, ApprovalRequest, ApprovalStore, GateOutcome};

    fn temp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cutex-older-stores-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn request(kind: ApprovalKind, sha: &str) -> ApprovalRequest {
        ApprovalRequest {
            kind,
            reason: "test".into(),
            requester_cutex_session_id: "cutex.a".into(),
            requester_name: None,
            project_id: None,
            summary: "s".into(),
            details: json!({}),
            request_sha256: sha.into(),
        }
    }

    #[test]
    fn static_root_approvals_are_removed_and_kept_aside() {
        let home = temp("approvals");
        let displaced = temp("displaced");
        let store = ApprovalStore::at(home.join(APPROVALS));
        let now = chrono::Utc::now();
        for (kind, sha) in [(ApprovalKind::StaticRoot, "a"), (ApprovalKind::ManagedService, "b")] {
            assert!(matches!(store.gate(request(kind, sha), now).unwrap(), GateOutcome::Pending { .. }));
        }
        let notes = prepare(&home, &displaced).unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        let left = crate::approvals::read_all_at(&home.join(APPROVALS)).unwrap();
        assert_eq!(left.iter().map(|r| r.kind).collect::<Vec<_>>(), vec![ApprovalKind::ManagedService]);
        let kept: serde_json::Value =
            serde_json::from_slice(&std::fs::read(displaced.join(OUT_DIR).join("approvals-static-root.json")).unwrap()).unwrap();
        assert_eq!(kept["approvals"].as_array().unwrap().len(), 1);
        // Nothing left to change: a second pass is a no-op.
        assert!(prepare(&home, &temp("displaced2")).unwrap().is_empty());
    }

    #[test]
    fn pending_claude_permission_modes_are_cleared_and_kept_aside() {
        let home = temp("sessions");
        let displaced = temp("displaced");
        let path = home.join(SESSIONS);
        let claude = |mode: Option<&str>| -> crate::runtime::claude::ClaudeSession {
            serde_json::from_value(json!({
                "deployment": {"python": "p", "adapter_source": "a", "claude_cli": "c", "claude_home": "h", "tui_binary": "t"},
                "thread_id": "t", "state_dir": "s", "model": "m", "initial_permission_mode": mode
            }))
            .unwrap()
        };
        crate::session::store::with_locked_session_rows(&path, |rows| {
            for (name, mode) in [("a", Some("auto")), ("b", None)] {
                let mut record = crate::session::model::CutexSessionRecord::new_at(
                    format!("cutex.{name}"), Some(format!("thread-{name}")), "devbox".into(), format!("/tmp/{name}"), None,
                    "2026-10-18T00:00:00Z".into(),
                )?;
                record.claude_session = Some(claude(mode));
                rows.insert_session(format!("cutex.{name}"), record);
            }
            rows.commit().map(|_| ())
        })
        .unwrap();
        let notes = prepare(&home, &displaced).unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        crate::session::store::with_cutex_session_store_at(&path, |store| {
            for record in store.sessions.values() {
                let claude = record.claude_session.as_ref().unwrap();
                assert!(claude.initial_permission_mode.is_none());
                // What an older cutex sees: no unknown field in the row.
                assert!(serde_json::to_value(claude).unwrap().get("initial_permission_mode").is_none());
            }
        })
        .unwrap();
        let kept: serde_json::Value = serde_json::from_slice(
            &std::fs::read(displaced.join(OUT_DIR).join("claude-initial-permission-modes.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(kept["sessions"][0]["initialPermissionMode"], "auto");
    }

    #[test]
    fn an_empty_home_needs_nothing() {
        assert!(prepare(&temp("empty"), &temp("displaced")).unwrap().is_empty());
    }
}
