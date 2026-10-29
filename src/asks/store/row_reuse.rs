//! One bounded serialized-row companion to the existing parsed snapshot.
//! Exact bytes (not hashes/timestamps) decide reuse; every changed row is decoded.
use super::*;
use crate::management::v2::hot_store::{RowChange, Rows};

const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_ROWS: usize = 100_000;

pub(super) fn bounded(rows: Rows) -> Option<Rows> {
    fits(&rows).then_some(rows)
}

pub(super) fn fits(rows: &Rows) -> bool {
    if rows.len() > MAX_ROWS { return false; }
    let bytes = rows.iter().try_fold(0usize, |sum, ((collection, key), value)| {
        sum.checked_add(collection.len())?.checked_add(key.len())?
            .checked_add(value.len())?.checked_add(96)
    });
    bytes.is_some_and(|bytes| bytes <= MAX_BYTES)
}

pub(super) fn changed(mut rows: Rows, changes: &[RowChange]) -> Option<Rows> {
    for change in changes {
        match change {
            RowChange::Put { collection, key, value } => {
                rows.insert((collection.clone(), key.clone()), value.clone());
            }
            RowChange::Delete { collection, key } => { rows.remove(&(collection.clone(), key.clone())); }
        }
    }
    bounded(rows)
}

pub(super) fn decode(rows: &Rows, previous: Option<(&AskState, &Rows)>) -> anyhow::Result<AskState> {
    let mut state = AskState::default();
    for (key, bytes) in rows {
        let prior = previous.filter(|(_, old)| old.get(key) == Some(bytes)).map(|(state, _)| state);
        macro_rules! map {
            ($field:ident) => {{
                let value = match prior.and_then(|state| state.$field.get(&key.1)) {
                    Some(value) => value.clone(),
                    None => serde_json::from_slice(bytes)
                        .with_context(|| format!("decode ask SQLite row {}/{}", key.0, key.1))?,
                };
                state.$field.insert(key.1.clone(), value);
            }};
        }
        macro_rules! singleton {
            ($field:ident) => {
                state.$field = match prior {
                    Some(state) => state.$field.clone(),
                    None => serde_json::from_slice(bytes)
                        .with_context(|| format!("decode ask SQLite singleton {}", key.1))?,
                }
            };
        }
        match key.0.as_str() {
            "peer_cancellations" => map!(peer_cancellations),
            "asks" => map!(asks), "answers" => map!(answers),
            "questions" => map!(questions), "deliveries" => map!(deliveries),
            "events" => map!(events), "leases" => map!(leases),
            "next_step_leases" => map!(next_step_leases), "pauses" => map!(pauses),
            "artifacts" => map!(artifacts), "waits" => map!(waits),
            "wait_cancel" => map!(wait_cancel), "actions" => map!(actions),
            "auto_turns" => map!(auto_turns), "presence" => map!(presence),
            "woken" => map!(woken), "owner_action" => map!(owner_action),
            "owner_action_told" => map!(owner_action_told), "facades" => map!(facades),
            "owner_offline" => {
                let offline: bool = serde_json::from_slice(bytes)?;
                anyhow::ensure!(offline, "invalid false owner-offline ask row");
                state.owner_offline.insert(key.1.clone());
            }
            "state" => match key.1.as_str() {
                "revision" => singleton!(revision), "next_seq" => singleton!(next_seq),
                "commands" => singleton!(commands), "notices" => singleton!(notices),
                "routing" => singleton!(routing),
                "project_control" => singleton!(project_control),
                "lineage" => singleton!(lineage),
                _ => anyhow::bail!("unknown ask SQLite row collection"),
            },
            _ => anyhow::bail!("unknown ask SQLite row collection"),
        }
    }
    Ok(state)
}
