//! Preserve the semantic bytes of queued messages across a formal rename.
//! Labels here are historical text snapshots, never addressing/authority.
use super::{inter_agent_params, inter_agent_semantic_sha256, ThreadInterAgentMessageParams};
use crate::agent_bus::model::AgentBusMessage;
use crate::management::v2::agent_bus_state::AgentBusMessageRepository;
use std::collections::BTreeSet;

fn proved(
    repository: &AgentBusMessageRepository,
    thread: &str,
    owner: &str,
    message: &AgentBusMessage,
    digest: &str,
    label: &str,
) -> anyhow::Result<Option<ThreadInterAgentMessageParams>> {
    let params = inter_agent_params(thread, label, owner, message)?;
    if inter_agent_semantic_sha256(&params) != digest {
        return Ok(None);
    }
    repository.remember_recipient_label(&message.id, owner, digest, label)?;
    Ok(Some(params))
}

fn variants(name: &str) -> Vec<String> {
    vec![
        name.to_owned(),
        crate::agent_bus::identity::sanitize_session_component(name, 48, "agent"),
    ]
}

pub fn durable_inter_agent_params(
    repository: &AgentBusMessageRepository,
    thread: &str,
    current_label: &str,
    owner: &str,
    message: &AgentBusMessage,
) -> anyhow::Result<ThreadInterAgentMessageParams> {
    let current = inter_agent_params(thread, current_label, owner, message)?;
    let Some(snapshot) = repository.snapshot_by_message_id(&message.id)? else {
        return Ok(current);
    };
    anyhow::ensure!(
        snapshot.to_cutex_session_id == owner,
        "queued message recipient changed"
    );
    let Some(digest) = snapshot.semantic_sha256.as_deref() else {
        return Ok(current);
    };
    if let Some(frozen) = repository.frozen_recipient_label(&message.id)? {
        let params = inter_agent_params(thread, &frozen, owner, message)?;
        anyhow::ensure!(
            inter_agent_semantic_sha256(&params) == digest,
            "frozen queued-message semantic content changed"
        );
        return Ok(params);
    }
    if inter_agent_semantic_sha256(&current) == digest {
        repository.remember_recipient_label(&message.id, owner, digest, current_label)?;
        return Ok(current);
    }
    // Older rows predate the snapshot. Test only same-recipient historical
    // text against their immutable SHA; a name never selects another SID.
    let mut tried = BTreeSet::from([current_label.to_owned()]);
    if let Some(name) = repository
        .message_meta(&message.id)?
        .and_then(|meta| meta.recipient)
        .filter(|party| party.id == owner)
        .and_then(|party| party.name)
    {
        for label in variants(&name) {
            if tried.insert(label.clone()) {
                if let Some(params) = proved(repository, thread, owner, message, digest, &label)? {
                    return Ok(params);
                }
            }
        }
    }
    if let Some(record) = crate::session::store::shared_session_row(owner)? {
        for name in record
            .formal_agent_name
            .iter()
            .chain(record.display_name_hint.iter())
            .chain(record.thread_name.iter())
        {
            for label in variants(name) {
                if tried.insert(label.clone()) {
                    if let Some(params) =
                        proved(repository, thread, owner, message, digest, &label)?
                    {
                        return Ok(params);
                    }
                }
            }
        }
    }
    let path = crate::session::store::cutex_sessions_path()?;
    let mut before = None;
    loop {
        let page = crate::session::store::formal_name_history_at(&path, owner, before, 100)?;
        for event in page.events {
            for name in [event.old_name, event.new_name] {
                for label in variants(&name) {
                    if tried.insert(label.clone()) {
                        if let Some(params) =
                            proved(repository, thread, owner, message, digest, &label)?
                        {
                            return Ok(params);
                        }
                    }
                }
            }
        }
        before = page.next;
        if before.is_none() {
            break;
        }
    }
    anyhow::bail!("queued message has no recipient-label snapshot or same-SID historical label matching its stored semantic SHA")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_bus::{
        delivery::AgentDeliveryMode,
        model::{AgentBusEnvelopeKind, AgentMessageKind},
    };
    use crate::management::v2::agent_bus_state::{
        AgentBusMessageMeta, AgentBusQueuedMessage, AgentParty,
    };

    #[test]
    fn rename_preserves_queued_semantic_bytes_and_legacy_proof() {
        let root = std::env::temp_dir().join(format!("rename-frozen-{}", uuid::Uuid::new_v4()));
        let repository = AgentBusMessageRepository::open(&root).unwrap();
        let owner = "cutex.frozen-recipient";
        for legacy in [false, true] {
            let message = AgentBusMessage {
                id: uuid::Uuid::new_v4().to_string(),
                kind: AgentBusEnvelopeKind::Message,
                from: "source-runtime".into(),
                to: "old-runtime".into(),
                from_cutex_session_id: Some("cutex.source".into()),
                to_cutex_session_id: Some(owner.into()),
                content: "unchanged message bytes".into(),
                delivery_mode: AgentDeliveryMode::Soon,
                trigger_turn: true,
                created_at_epoch_secs: chrono::Utc::now().timestamp() as u64,
                sender_kind: AgentMessageKind::Agent,
                display_source: None,
                submit_mode: None,
                control_type: None,
                control_payload: None,
                external_action_id: None,
                external_message_id: None,
            };
            let original = inter_agent_params("old-thread", "old-name", owner, &message).unwrap();
            let digest = inter_agent_semantic_sha256(&original);
            let queued = AgentBusQueuedMessage {
                owner_cutex_session_id: owner.into(),
                message_id: message.id.clone(),
                from_cutex_session_id: message.from_cutex_session_id.clone(),
                to_cutex_session_id: owner.into(),
                from_runtime_agent_id: Some(message.from.clone()),
                to_runtime_agent_id: Some(message.to.clone()),
                delivery_mode: "soon".into(),
                content: message.content.clone(),
                queued_at: chrono::Utc::now(),
                canonical_envelope: message.clone(),
                semantic_sha256: digest.clone(),
            };
            let meta = AgentBusMessageMeta {
                recipient: Some(AgentParty {
                    id: owner.into(),
                    name: Some("old-name".into()),
                    runtime: None,
                }),
                ..Default::default()
            };
            if legacy {
                repository.record_queued_with_meta(queued, meta).unwrap();
            } else {
                repository
                    .record_queued_with_meta_and_recipient_label(queued, meta, "old-name".into())
                    .unwrap();
            }
            let before = repository.canonical_message(&message.id).unwrap();
            let params =
                durable_inter_agent_params(&repository, "new-thread", "new-name", owner, &message)
                    .unwrap();
            assert_eq!(inter_agent_semantic_sha256(&params), digest);
            assert_eq!(
                repository
                    .frozen_recipient_label(&message.id)
                    .unwrap()
                    .as_deref(),
                Some("old-name")
            );
            assert_eq!(
                serde_json::to_value(repository.canonical_message(&message.id).unwrap()).unwrap(),
                serde_json::to_value(before).unwrap()
            );
            assert_eq!(
                repository.semantic_sha256(&message.id).unwrap().as_deref(),
                Some(digest.as_str())
            );
            let mut changed = message.clone();
            changed.content.push_str(" forged");
            assert!(durable_inter_agent_params(
                &repository,
                "new-thread",
                "new-name",
                owner,
                &changed
            )
            .is_err());
            assert!(durable_inter_agent_params(
                &repository,
                "new-thread",
                "new-name",
                "cutex.foreign",
                &message
            )
            .is_err());
        }
        drop(repository);
        // The hot store owns a cached SQLite connection; its test reset closes it.
        crate::management::v2::hot_store::clear_cached_generation();
        std::fs::remove_dir_all(root).unwrap();
    }
}
