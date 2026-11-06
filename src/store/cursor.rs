//! Opaque list and change cursors. A cursor pins the host instance, store,
//! epoch, scope (and filter); any mismatch is a resync, never a silent
//! cross-host or cross-epoch read.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};

use super::keyed::{ResyncReason, Scope, StoreError};
use super::manifest::StoreSpec;
use super::{Epoch, Seq, StoreHead};

const LIST_VERSION: u8 = 1;
const CHANGE_VERSION: u8 = 1;
const MAX_CURSOR_TEXT: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ListWire {
    v: u8,
    i: String,
    s: String,
    e: u64,
    c: String,
    f: u64,
    k: String,
    q: Seq,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ChangeWire {
    v: u8,
    i: String,
    s: String,
    e: u64,
    c: String,
    q: Seq,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListCursor {
    pub(crate) instance: String,
    pub(crate) store: String,
    pub(crate) epoch: Epoch,
    pub(crate) scope: Scope,
    pub(crate) filter: u64,
    pub(crate) after_key: String,
    pub(crate) snapshot_seq: Seq,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeCursor {
    pub(crate) instance: String,
    pub(crate) store: String,
    pub(crate) epoch: Epoch,
    pub(crate) scope: Scope,
    pub(crate) after_seq: Seq,
}

fn encode<T: Serialize>(wire: &T) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(wire).expect("cursor wire serializes"))
}

fn decode<T: for<'de> Deserialize<'de>>(text: &str) -> Result<T, StoreError> {
    if text.len() > MAX_CURSOR_TEXT {
        return Err(StoreError::Invalid("cursor too long".into()));
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(text)
        .map_err(|_| StoreError::Invalid("malformed cursor".into()))?;
    serde_json::from_slice(&bytes).map_err(|_| StoreError::Invalid("malformed cursor".into()))
}

impl ListCursor {
    pub(crate) fn new(
        instance: &str,
        spec: &StoreSpec,
        head: &StoreHead,
        scope: &Scope,
        filter: u64,
        after_key: String,
    ) -> Self {
        Self {
            instance: instance.into(),
            store: spec.id.into(),
            epoch: head.epoch,
            scope: scope.clone(),
            filter,
            after_key,
            snapshot_seq: head.seq,
        }
    }

    pub fn after_key(&self) -> &str {
        &self.after_key
    }

    pub fn snapshot_seq(&self) -> Seq {
        self.snapshot_seq
    }

    pub fn encode(&self) -> String {
        encode(&ListWire {
            v: LIST_VERSION,
            i: self.instance.clone(),
            s: self.store.clone(),
            e: self.epoch.0,
            c: self.scope.encode(),
            f: self.filter,
            k: self.after_key.clone(),
            q: self.snapshot_seq,
        })
    }

    /// Parses and checks the store id; instance/epoch/scope are checked by
    /// [`ListCursor::check`] against the live head.
    pub fn decode(spec: &StoreSpec, text: &str) -> Result<Self, StoreError> {
        let wire: ListWire = decode(text)?;
        if wire.v != LIST_VERSION {
            return Err(StoreError::Invalid("unsupported cursor version".into()));
        }
        if wire.s != spec.id {
            return Err(StoreError::Resync(ResyncReason::StoreMismatch));
        }
        Ok(Self {
            instance: wire.i,
            store: wire.s,
            epoch: Epoch(wire.e),
            scope: Scope::decode(&wire.c)?,
            filter: wire.f,
            after_key: wire.k,
            snapshot_seq: wire.q,
        })
    }

    pub(crate) fn check(
        &self,
        instance: &str,
        spec: &StoreSpec,
        head: &StoreHead,
        scope: &Scope,
        filter: u64,
    ) -> Result<(), StoreError> {
        let reason = if self.store != spec.id {
            Some(ResyncReason::StoreMismatch)
        } else if self.instance != instance {
            Some(ResyncReason::WrongInstance)
        } else if self.epoch != head.epoch {
            Some(ResyncReason::EpochChanged)
        } else if &self.scope != scope {
            Some(ResyncReason::ScopeMismatch)
        } else if self.filter != filter {
            Some(ResyncReason::FilterMismatch)
        } else {
            None
        };
        reason.map_or(Ok(()), |reason| Err(StoreError::Resync(reason)))
    }
}

impl ChangeCursor {
    pub(crate) fn new(instance: &str, spec: &StoreSpec, epoch: Epoch, scope: &Scope, after_seq: Seq) -> Self {
        Self {
            instance: instance.into(),
            store: spec.id.into(),
            epoch,
            scope: scope.clone(),
            after_seq,
        }
    }

    pub fn after_seq(&self) -> Seq {
        self.after_seq
    }

    pub fn encode(&self) -> String {
        encode(&ChangeWire {
            v: CHANGE_VERSION,
            i: self.instance.clone(),
            s: self.store.clone(),
            e: self.epoch.0,
            c: self.scope.encode(),
            q: self.after_seq,
        })
    }

    pub fn decode(spec: &StoreSpec, text: &str) -> Result<Self, StoreError> {
        let wire: ChangeWire = decode(text)?;
        if wire.v != CHANGE_VERSION {
            return Err(StoreError::Invalid("unsupported cursor version".into()));
        }
        if wire.s != spec.id {
            return Err(StoreError::Resync(ResyncReason::StoreMismatch));
        }
        Ok(Self {
            instance: wire.i,
            store: wire.s,
            epoch: Epoch(wire.e),
            scope: Scope::decode(&wire.c)?,
            after_seq: wire.q,
        })
    }

    /// Resync reason for this cursor against the live head, if any.
    pub(crate) fn resync_reason(
        &self,
        instance: &str,
        spec: &StoreSpec,
        head: &StoreHead,
        scope: &Scope,
    ) -> Option<ResyncReason> {
        if self.store != spec.id {
            Some(ResyncReason::StoreMismatch)
        } else if self.instance != instance {
            Some(ResyncReason::WrongInstance)
        } else if self.epoch != head.epoch {
            Some(ResyncReason::EpochChanged)
        } else if &self.scope != scope {
            Some(ResyncReason::ScopeMismatch)
        } else if self.after_seq > head.seq {
            // Ahead of the head: the history was replaced underneath.
            Some(ResyncReason::EpochChanged)
        } else if self.after_seq < head.low_water {
            Some(ResyncReason::BelowRetention)
        } else {
            None
        }
    }
}
