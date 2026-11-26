use super::{bounded, require};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Event {
    pub id: String,
    pub title: String,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Snapshot {
    pub source: String,
    pub observed_at: DateTime<Utc>,
    pub events: Vec<Event>,
}
impl Snapshot {
    pub(super) fn validate(&self) -> Result<()> {
        bounded(&self.source, 160)?;
        require(self.events.len() <= 1000, "invalid_calendar")?;
        let mut ids = BTreeSet::new();
        for event in &self.events {
            bounded(&event.id, 100)?;
            bounded(&event.title, 512)?;
            require(
                event.ends_at.timestamp_millis() > event.starts_at.timestamp_millis()
                    && ids.insert(&event.id),
                "invalid_calendar",
            )?;
        }
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Import {
    pub action_id: String,
    pub expected_revision: u64,
    pub snapshot: Snapshot,
}
