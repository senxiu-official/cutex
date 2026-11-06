//! Peer projection of the shared local/remote action target pin store.
use super::{
    store::{Peer, Store},
    Error, Result,
};
use crate::name_target_pins::{self as pins, Binding, PinError};
pub use pins::Key;
use serde_json::Value;

fn error(e: PinError) -> Error {
    Error::new(
        match e {
            PinError::Corrupt => "peer_store_corrupt",
            _ => e.code(),
        },
        match e {
            PinError::Conflict => 409,
            PinError::Invalid => 400,
            _ => 503,
        },
    )
}
fn unpack(binding: Binding) -> Result<(String, Value)> {
    let peer = binding
        .peer_id
        .ok_or_else(|| Error::new("idempotency_conflict", 409))?;
    if !super::valid_id(&peer)
        || !super::names::full_session_id(&binding.cutex_session_id)
        || binding.detail["cutexSessionId"].as_str() != Some(binding.cutex_session_id.as_str())
    {
        return Err(Error::new("peer_store_corrupt", 503));
    }
    Ok((peer, binding.detail))
}
pub fn lookup(store: &Store, key: &Key<'_>) -> Result<Option<(String, Value)>> {
    let root = store
        .dir
        .parent()
        .ok_or_else(|| Error::new("persistence_unavailable", 503))?;
    pins::lookup_at(root, key)
        .map_err(error)?
        .map(unpack)
        .transpose()
}
pub fn bind(store: &Store, key: &Key<'_>, peer: &Peer, target: &Value) -> Result<(String, Value)> {
    let id = target["cutexSessionId"]
        .as_str()
        .filter(|id| super::names::full_session_id(id))
        .ok_or_else(|| Error::new("peer_protocol_error", 502))?;
    let root = store
        .dir
        .parent()
        .ok_or_else(|| Error::new("persistence_unavailable", 503))?;
    let binding = Binding {
        peer_id: Some(peer.peer_id.clone()),
        cutex_session_id: id.into(),
        detail: target.clone(),
    };
    unpack(pins::bind_at(root, key, &binding).map_err(error)?)
}

/// Relationship permissions are current; a changed/reused label never replaces
/// the pinned immutable peer. The receiver independently admits its bound SID.
pub fn peer(store: &Store, id: &str) -> Result<Peer> {
    let state = store.snapshot()?;
    let peer = state
        .peers
        .get(id)
        .cloned()
        .ok_or_else(|| Error::new("peer_not_found", 404))?;
    if !state.settings.enabled
        || !peer.enabled
        || peer.pending_until.is_some()
        || peer.policy.outbound.visibility != super::policy::Visibility::All
    {
        return Err(Error::new("peer_unavailable", 409));
    }
    if matches!(
        peer.status.as_str(),
        "identityChanged" | "versionMismatch" | "removedByPeer"
    ) {
        return Err(Error::new("peer_unsupported", 409));
    }
    Ok(peer)
}
