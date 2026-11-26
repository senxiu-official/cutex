//! Bound transport for the Claude ordered inbox. Each member retains its
//! canonical digest and native status. A failed RPC grants no replay authority.

use super::{Delivery, DeliveryState, Envelope, MessageKey, Response, Status};
use crate::launch::stock::ExternalInputBinding;
use anyhow::{ensure, Context};
use serde_json::{json, Value};
use std::collections::BTreeSet;

pub const SUBMIT_METHOD: &str = "thread/externalInput/submitBatch";
pub const STATUS_METHOD: &str = "thread/externalInput/status";
pub const CAPACITY_METHOD: &str = "thread/externalInput/batchCapacity";
pub const MAX_MEMBERS: usize = 100;
pub const MAX_BYTES: usize = 1024 * 1024;

pub fn capacity_params(binding: &ExternalInputBinding) -> Value {
    json!({
        "version": 1,
        "ownerId": binding.owner_id,
        "threadId": binding.thread_id,
        "runtimeGeneration": binding.runtime_generation,
    })
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Capacity {
    version: u32,
    owner_id: String,
    thread_id: String,
    runtime_generation: u64,
    available_members: usize,
    available_bytes: usize,
}

pub fn validate_capacity(
    binding: &ExternalInputBinding,
    reply: Value,
) -> anyhow::Result<crate::app_server::pre_turn_notes::Budget> {
    let reply: Capacity = serde_json::from_value(reply)?;
    ensure!(
        reply.version == 1
            && reply.owner_id == binding.owner_id
            && reply.thread_id == binding.thread_id
            && reply.runtime_generation == binding.runtime_generation,
        "automatic input capacity binding changed"
    );
    ensure!(
        reply.available_members < MAX_MEMBERS && reply.available_bytes <= MAX_BYTES,
        "automatic input capacity exceeds transport limits"
    );
    Ok(crate::app_server::pre_turn_notes::Budget {
        notes: reply.available_members,
        bytes: reply.available_bytes,
    })
}

pub struct Batch {
    binding: ExternalInputBinding,
    members: Vec<Envelope>,
    keys: Vec<MessageKey>,
    trigger: Option<String>,
    notes_at_start: bool,
}

impl Batch {
    pub fn new(members: Vec<Envelope>, trigger: Option<String>) -> anyhow::Result<Self> {
        ensure!(
            (1..=MAX_MEMBERS).contains(&members.len()),
            "invalid automatic batch size"
        );
        let first = &members[0];
        let binding = ExternalInputBinding {
            version: 1,
            owner_id: first.owner_id.clone(),
            thread_id: first.thread_id.clone(),
            runtime_generation: first.runtime_generation,
            canonical_byte_limit: Default::default(),
        };
        let mut identities = BTreeSet::new();
        let mut remaining = MAX_BYTES;
        for member in &members {
            member.validate()?;
            ensure!(
                member.owner_id == binding.owner_id
                    && member.thread_id == binding.thread_id
                    && member.runtime_generation == binding.runtime_generation,
                "automatic batch has mixed runtime bindings"
            );
            ensure!(
                identities.insert(member.message.id.as_str()),
                "duplicate automatic batch identity"
            );
            let active = member.message.delivery != Delivery::Passive;
            ensure!(
                active == (trigger.as_deref() == Some(member.message.id.as_str())),
                "automatic batch delivery differs from trigger"
            );
            remaining = remaining
                .checked_sub(serde_json::to_vec(member)?.len())
                .context("automatic batch exceeds 1 MiB")?;
        }
        ensure!(
            trigger.as_deref().is_none_or(|id| identities.contains(id)),
            "automatic batch trigger absent"
        );
        let keys = members.iter().map(Envelope::key).collect();
        Ok(Self {
            binding,
            members,
            keys,
            trigger,
            notes_at_start: false,
        })
    }

    pub fn with_notes_at_start(mut self, enabled: bool) -> anyhow::Result<Self> {
        ensure!(
            !enabled || self.trigger.is_none(),
            "notes-at-start requires a passive batch"
        );
        self.notes_at_start = enabled;
        Ok(self)
    }

    fn binding_params(&self) -> Value {
        capacity_params(&self.binding)
    }

    pub fn submit_params(&self) -> Value {
        let mut params = self.binding_params();
        params["messages"] = json!(self.members);
        params["triggerId"] = json!(self.trigger);
        if self.notes_at_start {
            params["notesAtStart"] = json!(true);
        }
        params
    }

    pub fn status_params(&self) -> Value {
        let mut params = self.binding_params();
        params["messages"] = json!(self.keys);
        params
    }

    pub fn validate_status(&self, response: Value) -> anyhow::Result<Vec<Status>> {
        let response: Response = serde_json::from_value(response)?;
        response.validate(&self.binding, &self.keys)?;
        Ok(response.statuses)
    }

    pub fn validate_submit(&self, response: Value) -> anyhow::Result<Vec<Status>> {
        let statuses = self.validate_status(response)?;
        ensure!(
            statuses.iter().all(|status| matches!(
                status.delivery_state,
                DeliveryState::Pending | DeliveryState::ContextPersisted
            )),
            "automatic batch reply does not prove complete admission"
        );
        Ok(statuses)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_server::external_input::{Message, Source, SourceKind};

    fn member(id: &str, delivery: Delivery) -> Envelope {
        let mut value = Envelope {
            version: 1,
            owner_id: "owner".into(),
            thread_id: "thread".into(),
            runtime_generation: 7,
            message: Message {
                id: id.into(),
                source: Source {
                    kind: SourceKind::Agent,
                    id: "sender".into(),
                },
                event_type: "message".into(),
                delivery,
                text: format!("text of {id}"),
            },
            view: None,
            semantic_sha256: String::new(),
        };
        value.semantic_sha256 = value.digest();
        value
    }

    #[test]
    fn submit_reply_needs_all_members_and_never_creates_a_read_receipt() {
        let members = vec![
            member("note", Delivery::Passive),
            member("result", Delivery::Soon),
        ];
        let batch = Batch::new(members.clone(), Some("result".into())).unwrap();
        assert_eq!(
            batch.submit_params()["messages"][0]["message"]["id"],
            "note"
        );
        assert_eq!(
            batch.status_params()["messages"],
            json!(members.iter().map(Envelope::key).collect::<Vec<_>>())
        );
        let mut reply = json!({
            "version":1, "ownerId":"owner","threadId":"thread","runtimeGeneration":7,
            "statuses":members.iter().map(|member| json!({
                "messageId":member.message.id, "semanticSha256":member.semantic_sha256,
                "deliveryState":"pending", "receipt":null,
                "processing":{"state":"pending","attemptId":null,"reason":null},
            })).collect::<Vec<_>>()
        });
        assert!(batch
            .validate_submit(reply.clone())
            .unwrap()
            .iter()
            .all(|s| s.receipt.is_none()));
        reply["statuses"][0]["deliveryState"] = json!("unknown");
        assert!(batch.validate_status(reply.clone()).is_ok());
        assert!(batch.validate_submit(reply.clone()).is_err());
        reply["statuses"].as_array_mut().unwrap().reverse();
        assert!(batch.validate_status(reply).is_err());
    }

    #[test]
    fn a_passive_batch_cannot_trigger_or_change_binding() {
        let first = member("first", Delivery::Passive);
        assert!(Batch::new(vec![first.clone()], Some("first".into())).is_err());
        assert!(Batch::new(vec![member("first", Delivery::Soon)], None).is_err());
        let mut different = member("other", Delivery::Passive);
        different.runtime_generation += 1;
        assert!(Batch::new(vec![first.clone(), different], None).is_err());
        assert!(Batch::new(vec![first.clone(), first], None).is_err());
    }
}
