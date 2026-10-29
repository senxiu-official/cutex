//! Delegation lineage lives beside asks in the SAME AskStore transaction.
//! This module owns no database, runtime, network or store lock. The parent
//! persists `LineageState` in AskState and supplies authenticated read scope.
//! Creation and admission must check persistent ancestor stop fences while
//! holding that same transaction; legacy parent strings are never proof.

mod creation;
mod projection;
mod stop;

pub use creation::*;
pub use projection::*;
pub use stop::*;
pub use super::routing_policy::RoutingRef as DelegationRef;

use std::collections::{BTreeMap, BTreeSet};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use super::store::AskError;

pub const MAX_LINEAGE_NODES: usize = 10_000;
pub const MAX_LINEAGE_DEPTH: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="camelCase")]
pub struct ParentOccurrence {
    pub reference: DelegationRef,
    pub attempt_id: String,
    pub owner_session_id: String,
    pub runtime_generation: u64,
    pub turn_id: String,
    pub source_event_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag="state",content="occurrence",rename_all="camelCase")]
pub enum ParentFact { None, Verified(ParentOccurrence), Unknown }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="camelCase")]
pub struct LineageRecord {
    pub reference: DelegationRef,
    pub parent: ParentFact,
    pub root: Option<DelegationRef>,
    pub follow_up: Option<DelegationRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="camelCase")]
pub struct StopFence {
    pub root: DelegationRef,
    pub action_id: String,
    pub requested_by: String,
    pub descendants: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="camelCase")]
pub struct LineageState {
    pub revision: u64,
    /// Keys encode the full (host,id) tuple, not a bare id or alias.
    #[serde(default)] pub records: BTreeMap<String,LineageRecord>,
    /// Persistent, never cleared by resume or retry. New work is a new ask.
    #[serde(default)] pub stop_fences: BTreeMap<String,StopFence>,
}

pub fn reference_key(reference: &DelegationRef) -> String {
    serde_json::to_string(&(reference.host_id(),reference.id())).expect("bounded string tuple")
}

impl LineageState {
    pub fn get(&self, reference:&DelegationRef) -> Option<&LineageRecord> {
        self.records.get(&reference_key(reference)).filter(|record| &record.reference==reference)
    }

    /// Parent invokes this alongside ask insertion under the existing lock.
    /// Replayed identical metadata does nothing; conflicting ancestry refuses.
    pub fn record(&mut self, record:LineageRecord) -> Result<bool,AskError> {
        if let Some(old)=self.get(&record.reference) {
            return if old==&record {Ok(false)} else {Err(error("lineage_conflict"))};
        }
        self.check_creation_fences(&record)?;
        let next=self.revision.checked_add(1).ok_or_else(||error("revision_overflow"))?;
        self.records.insert(reference_key(&record.reference),record);
        self.revision=next;
        Ok(true)
    }

    /// Pure read guard, also required immediately before native admission.
    pub fn check_creation_fences(&self, record:&LineageRecord) -> Result<(),AskError> {
        let mut cursor=Some(record.reference.clone());
        let mut seen=BTreeSet::new();
        for _ in 0..MAX_LINEAGE_DEPTH {
            let Some(reference)=cursor.take() else {return Ok(());};
            if !seen.insert(reference.clone()) {return Err(error("lineage_cycle"));}
            if let Some(fence)=self.stop_fences.get(&reference_key(&reference)) {
                if reference==record.reference || fence.descendants {return Err(error("stop_fenced"));}
            }
            let current=if reference==record.reference {Some(record)} else {self.get(&reference)};
            if let Some(root)=current.and_then(|r|r.root.as_ref()) {
                if self.stop_fences.get(&reference_key(root)).is_some_and(|f|f.descendants) {
                    return Err(error("stop_fenced"));
                }
            }
            cursor=current.and_then(|r|match &r.parent {ParentFact::Verified(p)=>Some(p.reference.clone()),_=>None});
        }
        Err(error("lineage_depth"))
    }
}

pub(crate) fn error(code:&'static str) -> AskError { AskError::new(code,"delegation lineage precondition failed") }

#[cfg(test)]
mod tests;

#[cfg(test)]
mod bounds_tests;
