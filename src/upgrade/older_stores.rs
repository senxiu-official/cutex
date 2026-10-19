//! Rollback swap step (services stopped): rewrite the few records an older
//! cutex cannot parse at all, or would read with more rights than they
//! carry, so it loads the rest of its stores and never widens access. Every
//! record changed here is copied to `displaced/older-stores/` first (never
//! deleted); a roll-forward does not need them back.
//!
//! Each step names the release that introduced its records (`since`) and
//! runs when the rollback target is older, or is not a plain `N.N.N`
//! release label (dev builds, `rN` labels, `unknown`: every step runs; each
//! rewrite is readable by every release, this one included).
//!
//! Since 0.1.4:
//! - Approval records of kind `static_root`: an older release rejects the
//!   whole `runtime/approvals/approvals.json` ("approval store is corrupt").
//!   They are removed; a pending one is simply asked again.
//! - `ClaudeSession.initial_permission_mode` (pending until the agent's
//!   first successful start): `ClaudeSession` is `deny_unknown_fields`, so an
//!   older release cannot load any session. The field is cleared; the agent
//!   then starts in the Profile's default mode, as it would have on that
//!   release.
//!
//! Since 0.1.5:
//! - Client / device tokens with a device scope other than `full`
//!   (`management-clients/tokens.json`): an older release ignores `scope`, so
//!   the device would get its user's full rights. Each is held
//!   (`client_tokens::ROLLBACK_HOLD_PREFIX`): the older release refuses it
//!   (401) and keeps the hold when it rewrites the store; 0.1.5 and later read
//!   it again with its scope, so the device works again after a roll-forward
//!   without re-pairing. Kept aside: `client-tokens-held.json` (ids, labels,
//!   scopes; never a secret or hash).
//! - Pending cross-host wakes (`cutex.peer.tell.v2` in
//!   `runtime/management-v2/agent-bus-message-state.json`) never handed to a
//!   runtime: an older release never delivers the unknown control type but
//!   lists the wake as an eligible next step forever (owner input held ~30 s
//!   per message, new delegations to the agent held). Each becomes the
//!   cross-host note it would have been without `wake` (read at the
//!   recipient's next turn; no turn is started).
//! - Unacknowledged new owner device notices (`newOwnerDevices` in
//!   `runtime/users/users-v1.json`): an older release drops them on its next
//!   write of the store. Only copied aside (`new-owner-devices.json`), so the
//!   owner can still review those devices; the store is not changed.

use std::cmp::Ordering;
use std::path::Path;

use serde_json::json;

const APPROVALS: &str = "runtime/approvals/approvals.json";
const SESSIONS: &str = "cutex-sessions.json";
const CLIENT_TOKENS: &str = "management-clients";
const AGENT_BUS: &str = "runtime/management-v2";
const AGENT_BUS_STATE: &str = "runtime/management-v2/agent-bus-message-state.json";
const USERS: &str = "runtime/users/users-v1.json";
const OUT_DIR: &str = "older-stores";

/// Whether the records introduced in `since` must be rewritten for `target`.
fn older_than(target: &str, since: &str) -> bool {
    let plain = !target.is_empty()
        && target.split('.').count() == 3
        && target.split('.').all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
    !plain || crate::upgrade::manifest::compare_releases(target, since) == Ordering::Less
}

/// `target`: the release label the rollback switches to.
pub fn prepare(home: &Path, displaced: &Path, target: &str) -> anyhow::Result<Vec<String>> {
    let mut notes = Vec::new();
    if older_than(target, "0.1.4") {
        since_0_1_4(home, displaced, &mut notes)?;
    }
    if older_than(target, "0.1.5") {
        since_0_1_5(home, displaced, &mut notes)?;
    }
    Ok(notes)
}

fn since_0_1_4(home: &Path, displaced: &Path, notes: &mut Vec<String>) -> anyhow::Result<()> {
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
    Ok(())
}

fn since_0_1_5(home: &Path, displaced: &Path, notes: &mut Vec<String>) -> anyhow::Result<()> {
    let held = crate::management::client_tokens::ClientTokenStore::at(home.join(CLIENT_TOKENS))
        .hold_narrowed_for_rollback()?;
    if !held.is_empty() {
        save(displaced, "client-tokens-held.json", &json!({ "tokens": held }))?;
        notes.push(format!(
            "{CLIENT_TOKENS}/tokens.json: {} device token(s) with a narrowed scope held (refused until the roll-forward)",
            held.len()
        ));
    }
    if home.join(AGENT_BUS_STATE).is_file() {
        let downgraded =
            crate::management::v2::agent_bus_state::AgentBusMessageRepository::open(home.join(AGENT_BUS))?
                .downgrade_pending_peer_wakes()?;
        if !downgraded.is_empty() {
            save(displaced, "peer-wakes.json", &json!({ "messages": downgraded }))?;
            notes.push(format!("{AGENT_BUS_STATE}: {} pending cross-host wake(s) turned into notes", downgraded.len()));
        }
    }
    let users = home.join(USERS);
    if users.is_file() {
        let state: serde_json::Value = serde_json::from_slice(&std::fs::read(&users)?)
            .map_err(|error| anyhow::anyhow!("read {}: {error}", users.display()))?;
        let notices = state.get("newOwnerDevices").and_then(|v| v.as_array()).filter(|v| !v.is_empty());
        if let Some(notices) = notices {
            save(displaced, "new-owner-devices.json", &json!({ "newOwnerDevices": notices }))?;
            notes.push(format!(
                "{USERS}: {} unacknowledged new owner device notice(s) kept aside (the older release drops them)",
                notices.len()
            ));
        }
    }
    Ok(())
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
        let notes = prepare(&home, &displaced, "0.1.3").unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        let left = crate::approvals::read_all_at(&home.join(APPROVALS)).unwrap();
        assert_eq!(left.iter().map(|r| r.kind).collect::<Vec<_>>(), vec![ApprovalKind::ManagedService]);
        let kept: serde_json::Value =
            serde_json::from_slice(&std::fs::read(displaced.join(OUT_DIR).join("approvals-static-root.json")).unwrap()).unwrap();
        assert_eq!(kept["approvals"].as_array().unwrap().len(), 1);
        // Nothing left to change: a second pass is a no-op.
        assert!(prepare(&home, &temp("displaced2"), "0.1.3").unwrap().is_empty());
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
        let notes = prepare(&home, &displaced, "0.1.3").unwrap();
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
        for target in ["0.1.3", "0.1.4", "dev-4add630", "unknown"] {
            let home = temp("empty");
            assert!(prepare(&home, &temp("displaced"), target).unwrap().is_empty());
            assert_eq!(std::fs::read_dir(&home).unwrap().count(), 0, "nothing created for {target}");
        }
    }

    #[test]
    fn steps_run_for_older_or_unparsed_targets_only() {
        assert!(older_than("0.1.3", "0.1.4") && older_than("0.1.4", "0.1.5") && older_than("0.1.9", "0.1.10"));
        assert!(!older_than("0.1.4", "0.1.4") && !older_than("0.1.5", "0.1.4") && !older_than("0.2.0", "0.1.5"));
        // Not a plain release label: every step runs (each rewrite is readable by all).
        for target in ["dev-4add630", "r245", "unknown", "", "0.1.4-rc1", "0.1"] {
            assert!(older_than(target, "0.1.4") && older_than(target, "0.1.5"), "{target}");
        }
    }

    #[test]
    fn a_0_1_4_target_keeps_what_0_1_4_reads() {
        let home = temp("keep-014");
        let store = ApprovalStore::at(home.join(APPROVALS));
        assert!(matches!(
            store.gate(request(ApprovalKind::StaticRoot, "a"), chrono::Utc::now()).unwrap(),
            GateOutcome::Pending { .. }
        ));
        assert!(prepare(&home, &temp("displaced"), "0.1.4").unwrap().is_empty());
        assert_eq!(crate::approvals::read_all_at(&home.join(APPROVALS)).unwrap().len(), 1);
        // A 0.1.3 target (and a dev label) still removes it.
        assert_eq!(prepare(&home, &temp("displaced"), "dev-4add630").unwrap().len(), 1);
    }

    #[test]
    fn narrowed_device_tokens_are_held_for_releases_that_ignore_scopes() {
        use crate::management::client_tokens::{ClientTokenStore, DeviceScope};
        let home = temp("tokens");
        let tokens = ClientTokenStore::at(home.join(CLIENT_TOKENS));
        let full = tokens.issue("desktop").unwrap();
        let (chat, _) = tokens.issue_device_scoped("phone", "Pixel", "android", DeviceScope::Chat).unwrap();
        // A 0.1.5 target reads scopes: nothing to do.
        assert!(prepare(&home, &temp("displaced"), "0.1.5").unwrap().is_empty());
        let displaced = temp("displaced");
        let notes = prepare(&home, &displaced, "0.1.4").unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(tokens.tokens_path()).unwrap()).unwrap();
        let hash = crate::management::client_tokens::token_hash_hex;
        assert_eq!(raw["tokens"][0]["tokenSha256"], hash(&full.token).as_str(), "a full token stays usable");
        assert_ne!(raw["tokens"][1]["tokenSha256"], hash(&chat.token).as_str(), "0.1.4 cannot match the held one");
        let kept: serde_json::Value =
            serde_json::from_slice(&std::fs::read(displaced.join(OUT_DIR).join("client-tokens-held.json")).unwrap()).unwrap();
        assert_eq!(kept["tokens"][0]["tokenId"], chat.token_id.as_str());
        assert_eq!(kept["tokens"][0]["scope"], "chat");
        assert!(!kept.to_string().contains(&hash(&chat.token)), "no hash kept aside");
        assert!(prepare(&home, &temp("displaced2"), "0.1.4").unwrap().is_empty(), "a second pass is a no-op");
        // After a roll-forward the device is back with its scope.
        assert_eq!(tokens.list().unwrap()[1].scope, DeviceScope::Chat);
        assert_eq!(tokens.verify(&chat.token).unwrap(), Some(chat.token_id));
    }

    fn peer_message(id: &str, wake: bool) -> crate::management::v2::agent_bus_state::AgentBusQueuedMessage {
        use crate::agent_bus::peer_tell::{CONTROL_TYPE, CONTROL_TYPE_V2};
        const TO: &str = "cutex.11111111-1111-4111-8111-111111111111";
        let envelope: crate::agent_bus::model::AgentBusMessage = serde_json::from_value(json!({
            "id": id, "kind": "message", "from": "worker@alpha", "to": "helper",
            "toCutexSessionId": TO, "content": "see /tmp/x",
            "deliveryMode": if wake { "after_turn" } else { "passive" }, "triggerTurn": wake,
            "createdAtEpochSecs": 1, "senderKind": "agent",
            "controlType": if wake { CONTROL_TYPE_V2 } else { CONTROL_TYPE },
            "controlPayload": {"peerId": "ci-0123456789abcdef0123456789abcdef", "host": "alpha",
                "senderId": "cutex.remote", "senderName": "worker", "tellId": format!("ptl-{id}")},
            "externalMessageId": format!("ptell-{id}")
        }))
        .unwrap();
        let params = crate::app_server::bus_bridge::inter_agent_params("", "", TO, &envelope).unwrap();
        crate::management::v2::agent_bus_state::AgentBusQueuedMessage {
            owner_cutex_session_id: TO.into(),
            message_id: id.into(),
            from_cutex_session_id: None,
            to_cutex_session_id: TO.into(),
            from_runtime_agent_id: None,
            to_runtime_agent_id: Some("rt-to".into()),
            delivery_mode: envelope.delivery_mode.event_label().into(),
            content: envelope.content.clone(),
            queued_at: chrono::Utc::now(),
            semantic_sha256: crate::app_server::bus_bridge::inter_agent_semantic_sha256(&params),
            canonical_envelope: envelope,
        }
    }

    #[test]
    fn pending_cross_host_wakes_become_notes_for_releases_without_wakes() {
        use crate::agent_bus::delivery::AgentDeliveryMode;
        use crate::agent_bus::peer_tell::CONTROL_TYPE;
        use crate::management::v2::agent_bus_state::AgentBusMessageRepository;
        let home = temp("wakes");
        let repository = AgentBusMessageRepository::open(home.join(AGENT_BUS)).unwrap();
        repository.record_queued_isolated(peer_message("wake-1", true)).unwrap();
        repository.record_queued_isolated(peer_message("note-1", false)).unwrap();
        let note_before = repository.snapshot_by_message_id("note-1").unwrap().unwrap();
        assert!(prepare(&home, &temp("displaced"), "0.1.5").unwrap().is_empty());
        let displaced = temp("displaced");
        let notes = prepare(&home, &displaced, "0.1.4").unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        // Read fresh, as the older release would.
        let repository = AgentBusMessageRepository::open(home.join(AGENT_BUS)).unwrap();
        let wake = repository.canonical_message("wake-1").unwrap();
        assert_eq!(wake.control_type.as_deref(), Some(CONTROL_TYPE));
        assert_eq!((wake.delivery_mode.clone(), wake.trigger_turn), (AgentDeliveryMode::Passive, false));
        assert_eq!(wake.external_message_id.as_deref(), Some("ptell-wake-1"));
        let snapshot = repository.snapshot_by_message_id("wake-1").unwrap().unwrap();
        assert_eq!((snapshot.state.as_str(), snapshot.delivery_mode.as_str()), ("pending", "passive"));
        // The digest is the note's: what a v1-only release recomputes.
        let (_, text) = crate::agent_bus::peer_tell::model_text(&wake).unwrap();
        assert!(!text.contains("Wake:"), "{text}");
        let params = crate::app_server::bus_bridge::inter_agent_params("", "", &snapshot.to_cutex_session_id, &wake).unwrap();
        assert_eq!(snapshot.semantic_sha256.as_deref(), Some(crate::app_server::bus_bridge::inter_agent_semantic_sha256(&params).as_str()));
        assert_eq!(repository.snapshot_by_message_id("note-1").unwrap().unwrap(), note_before, "a note stays");
        let kept: serde_json::Value =
            serde_json::from_slice(&std::fs::read(displaced.join(OUT_DIR).join("peer-wakes.json")).unwrap()).unwrap();
        assert_eq!(kept["messages"][0]["messageId"], "wake-1");
        assert_eq!(kept["messages"][0]["canonicalEnvelope"]["controlType"], "cutex.peer.tell.v2");
        assert!(prepare(&home, &temp("displaced2"), "0.1.4").unwrap().is_empty(), "a second pass is a no-op");
    }

    #[test]
    fn unacknowledged_new_owner_device_notices_are_kept_aside() {
        let home = temp("notices");
        let users = home.join(USERS);
        std::fs::create_dir_all(users.parent().unwrap()).unwrap();
        let state = json!({"schema": "cutex/users/v1", "newOwnerDevices": [
            {"tokenId": "mct-1", "at": "2026-10-20T00:00:00Z", "via": "pairing", "announced": true}]});
        std::fs::write(&users, serde_json::to_vec(&state).unwrap()).unwrap();
        let displaced = temp("displaced");
        assert!(prepare(&home, &temp("displaced"), "0.1.5").unwrap().is_empty());
        assert_eq!(prepare(&home, &displaced, "0.1.4").unwrap().len(), 1);
        let kept: serde_json::Value =
            serde_json::from_slice(&std::fs::read(displaced.join(OUT_DIR).join("new-owner-devices.json")).unwrap()).unwrap();
        assert_eq!(kept["newOwnerDevices"][0]["tokenId"], "mct-1");
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&std::fs::read(&users).unwrap()).unwrap(), state, "store unchanged");
    }
}
