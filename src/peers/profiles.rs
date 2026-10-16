//! Peer owner profiles (capability `peer-owner-profile`, optional hello
//! feature [`FEATURE`]).
//!
//! A host serves its owner's directly-set display name and avatar to paired
//! peers; a host that holds a local user bound to that owner's person key
//! (`peerPersons` from pairing, see `management::persons`) pulls them and
//! stores them as peer-sourced fields of that user's profile
//! (`management::user_profiles::Profile::peer`). A value the person set or
//! cleared directly on this host (`PATCH /v2/users/me`) always wins.
//!
//! Wire (authenticated like every `/peer/v1/*` route):
//!
//! - `GET /peer/v1/owner-profile` → `{schemaVersion: 1, etag, displayName:
//!   string|null, avatar: {etag, contentType, bytes, width, height}|null}`;
//!   `etag` = the first 16 hex digits of SHA-256 over the name and avatar
//!   ETag (also in hello as `ownerProfile.etag`, so a heartbeat notices a
//!   change without fetching the profile).
//! - `GET /peer/v1/owner-profile/avatar` → `{etag, contentType, data}`
//!   (`data` base64); 404 `not_found` without an avatar.
//!
//! Only directly-set fields are served, never values the serving host got
//! from another peer, so profiles never travel more than one hop and two
//! hosts of the same person cannot echo stale values back and forth.
//!
//! Pulling happens in the peer worker: once per process start / pairing,
//! after a heartbeat (directory refresh) whose hello carried another profile
//! ETag or when the set of linked users changed, and at least every
//! [`RESYNC`]. Avatar bytes are fetched only when the ETag differs from the
//! stored one. Peers disabled (globally or per peer) or no linked user: no
//! request, no store write.

use super::{
    store::{Peer, Store},
    Error, Result,
};
use crate::management::user_profiles::{self, PeerAvatar, PeerProfile, ProfileStore, FIELD_AVATAR, FIELD_DISPLAY_NAME};
use crate::management::users::UserStore;
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const FEATURE: &str = "owner-profile.v1";
pub const CAPABILITY: &str = "peer-owner-profile";
pub const ROUTE: &str = "/peer/v1/owner-profile";
pub const AVATAR_ROUTE: &str = "/peer/v1/owner-profile/avatar";
pub const USERS_CHANGED: &str = "cutex/users/changed";
/// Fallback re-check interval (a lost hello / heartbeat still converges).
pub const RESYNC: Duration = Duration::from_secs(30 * 60);

// ---------------------------------------------------------------- serving

fn profile_etag(display_name: Option<&str>, avatar_etag: Option<&str>) -> String {
    let digest = Sha256::digest(json!([display_name, avatar_etag]).to_string().as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn owner_direct(profiles: &ProfileStore) -> user_profiles::Profile {
    profiles.profile(crate::management::users::OWNER).direct_only()
}

/// `GET /peer/v1/owner-profile` from `profiles`.
pub fn served(profiles: &ProfileStore) -> Value {
    let profile = owner_direct(profiles);
    json!({
        "schemaVersion": 1,
        "etag": profile_etag(profile.display_name.as_deref(), profile.avatar_etag()),
        "displayName": profile.display_name,
        "avatar": profile.avatar.as_ref().map(|a| json!({
            "etag": a.etag, "contentType": a.content_type, "bytes": a.bytes, "width": a.width, "height": a.height,
        })),
    })
}

/// `GET /peer/v1/owner-profile/avatar` from `profiles`.
pub fn served_avatar(profiles: &ProfileStore) -> Result<Value> {
    let owner = crate::management::users::OWNER;
    if owner_direct(profiles).avatar.is_none() {
        return Err(Error::new("not_found", 404));
    }
    let (avatar, bytes) = profiles.avatar(owner).ok_or_else(|| Error::new("not_found", 404))?;
    Ok(json!({
        "etag": avatar.etag,
        "contentType": avatar.content_type,
        "data": base64::engine::general_purpose::STANDARD.encode(bytes),
    }))
}

/// `ownerProfile` of hello (best effort: absent when the store is unreadable).
pub fn hello_field() -> Option<Value> {
    let profiles = ProfileStore::open_default().ok()?;
    let state = profiles.snapshot().ok()?;
    let profile = state.profile(crate::management::users::OWNER).cloned().unwrap_or_default().direct_only();
    Some(json!({ "etag": profile_etag(profile.display_name.as_deref(), profile.avatar_etag()) }))
}

pub fn handle(path: &str) -> Result<Value> {
    let profiles = ProfileStore::open_default()?;
    match path {
        ROUTE => Ok(served(&profiles)),
        _ => served_avatar(&profiles),
    }
}

// ---------------------------------------------------------------- pulling

#[derive(Clone, Debug, PartialEq, Eq)]
struct Synced {
    etag: Option<String>,
    users: Vec<String>,
    at: Instant,
}

fn synced() -> &'static Mutex<BTreeMap<String, Synced>> {
    static SYNCED: OnceLock<Mutex<BTreeMap<String, Synced>>> = OnceLock::new();
    SYNCED.get_or_init(Default::default)
}

fn hello_etags() -> &'static Mutex<BTreeMap<String, String>> {
    static ETAGS: OnceLock<Mutex<BTreeMap<String, String>>> = OnceLock::new();
    ETAGS.get_or_init(Default::default)
}

/// Remember the profile ETag a peer's hello carried (heartbeat).
pub fn note_hello(peer_id: &str, hello: &Value) {
    let etag = hello.pointer("/ownerProfile/etag").and_then(Value::as_str).filter(|e| valid_etag(e));
    let mut etags = hello_etags().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match etag {
        Some(etag) => {
            etags.insert(peer_id.to_string(), etag.to_string());
        }
        None => {
            etags.remove(peer_id);
        }
    }
}

fn valid_etag(etag: &str) -> bool {
    etag.len() == 16 && etag.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Whether the peer may be pulled from at all (no I/O beyond the snapshot).
fn active(state: &super::store::State, peer: &Peer) -> bool {
    state.settings.enabled
        && peer.enabled
        && peer.pending_until.is_none()
        && peer.status != "removedByPeer"
        && peer.negotiated_features.iter().any(|f| f == FEATURE)
}

/// Local users that hold one of `peer_id`'s owner keys. Reads the persons
/// store first and touches the users store only when the peer sent keys.
fn linked_users(users: &UserStore, peer_id: &str) -> Vec<String> {
    let Ok(persons) = users.persons_snapshot() else {
        return vec![];
    };
    let Some(keys) = persons.peer_persons.get(peer_id).filter(|keys| !keys.is_empty()) else {
        return vec![];
    };
    let Ok(state) = users.snapshot() else {
        return vec![];
    };
    let linked: BTreeSet<String> = keys.iter().filter_map(|key| persons.user_of_key(&state, key)).collect();
    linked.into_iter().collect()
}

/// Whether `other_peer` still vouches for `user` as its owner (then its
/// values stay; otherwise another peer may take over).
fn still_vouches(store: &Store, users: &UserStore, other_peer: &str, user: &str) -> bool {
    store
        .snapshot()
        .ok()
        .and_then(|state| state.peers.get(other_peer).map(|peer| active(&state, peer)))
        .unwrap_or(false)
        && linked_users(users, other_peer).iter().any(|linked| linked == user)
}

fn parse_profile(value: &Value) -> Result<(Option<String>, Option<String>)> {
    let protocol = || Error::new("peer_protocol_error", 502);
    if value.get("schemaVersion").and_then(Value::as_u64).unwrap_or(1) != 1 {
        return Err(Error::new("peer_unsupported", 409));
    }
    let name = match value.get("displayName") {
        None | Some(Value::Null) => None,
        Some(Value::String(name)) => Some(user_profiles::validate_display_name(name).map_err(|_| protocol())?),
        Some(_) => return Err(protocol()),
    };
    let etag = match value.get("avatar") {
        None | Some(Value::Null) => None,
        Some(avatar) => {
            let etag = avatar.get("etag").and_then(Value::as_str).filter(|e| valid_etag(e)).ok_or_else(protocol)?;
            Some(etag.to_string())
        }
    };
    Ok((name, etag))
}

/// Pull `peer_id`'s owner profile into its linked users. `fetch(path)` does
/// the authenticated GET. Returns the users whose profile changed (the
/// caller publishes [`USERS_CHANGED`]).
pub fn sync_with(
    store: &Store,
    users: &UserStore,
    profiles: &ProfileStore,
    peer_id: &str,
    fetch: &dyn Fn(&str) -> Result<Value>,
) -> Result<Vec<String>> {
    let state = store.snapshot()?;
    let Some(peer) = state.peers.get(peer_id).filter(|peer| active(&state, peer)) else {
        return Ok(vec![]);
    };
    let linked = linked_users(users, &peer.peer_id);
    if linked.is_empty() {
        return Ok(vec![]);
    }
    let remote = fetch(ROUTE)?;
    let (display_name, avatar_etag) = parse_profile(&remote)?;
    let mut fetched: Option<user_profiles::AvatarUpload> = None;
    let mut changed = Vec::new();
    for user in &linked {
        let local = profiles.profile(user);
        let avatar = match &avatar_etag {
            None => PeerAvatar::None,
            // Unchanged, or a field this peer may not write: no bytes needed.
            Some(etag) if local.avatar_etag() == Some(etag.as_str()) || !local.peer_may_write(FIELD_AVATAR) => {
                PeerAvatar::Same(etag.clone())
            }
            Some(etag) => {
                if fetched.is_none() {
                    let value = fetch(AVATAR_ROUTE)?;
                    let content_type = value.get("contentType").and_then(Value::as_str).unwrap_or_default();
                    if value.get("etag").and_then(Value::as_str) != Some(etag.as_str()) {
                        // The avatar changed between the two requests: retry
                        // at the next heartbeat.
                        return Err(Error::new("peer_unavailable", 503));
                    }
                    let data = value.get("data").and_then(Value::as_str).unwrap_or_default();
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(data)
                        .map_err(|_| Error::new("peer_protocol_error", 502))?;
                    fetched = Some(user_profiles::AvatarUpload { content_type: content_type.to_string(), bytes });
                }
                PeerAvatar::New(fetched.clone().unwrap())
            }
        };
        // The bytes must be what the peer announced (same validation as a
        // direct upload; the ETag is recomputed from the bytes).
        if let PeerAvatar::New(upload) = &avatar {
            let (checked, _) = user_profiles::validate_avatar(upload).map_err(|_| Error::new("peer_protocol_error", 502))?;
            if Some(checked.etag.as_str()) != avatar_etag.as_deref() {
                return Err(Error::new("peer_protocol_error", 502));
            }
        }
        if !local.peer_may_write(FIELD_DISPLAY_NAME) && !local.peer_may_write(FIELD_AVATAR) {
            continue;
        }
        let update = PeerProfile { display_name: display_name.clone(), avatar };
        let did = profiles
            .apply_peer(user, &peer.peer_id, update, |other| !still_vouches(store, users, other, user))
            .map_err(|_| Error::new("persistence_unavailable", 503))?;
        if did {
            changed.push(user.clone());
        }
    }
    Ok(changed)
}

/// The worker's pull decision: the first round always; otherwise only after
/// a heartbeat whose hello ETag differs from the last pull (or carried none),
/// when the linked users changed, or after [`RESYNC`].
fn needs_sync(
    previous: Option<&Synced>,
    hello: Option<&str>,
    refreshed: bool,
    linked_now: impl FnOnce() -> Option<Vec<String>>,
) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    if previous.at.elapsed() >= RESYNC {
        return true;
    }
    if !refreshed {
        return false;
    }
    if hello.is_none() || previous.etag.as_deref() != hello {
        return true;
    }
    linked_now().is_some_and(|linked| linked != previous.users)
}

/// Peer worker hook (after the per-peer probe / refresh step). `refreshed`
/// = this round fetched hello. Never fails the caller.
pub fn maybe_sync(store: &Store, peer_id: &str, refreshed: bool) {
    let previous = synced().lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(peer_id).cloned();
    let hello = hello_etags().lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(peer_id).cloned();
    let linked_now = || UserStore::open_default().map(|users| linked_users(&users, peer_id)).ok();
    if !needs_sync(previous.as_ref(), hello.as_deref(), refreshed, linked_now) {
        return;
    }
    let Ok(state) = store.snapshot() else { return };
    let Some(peer) = state.peers.get(peer_id).filter(|peer| active(&state, peer)).cloned() else {
        // E.g. an older peer without the feature: look again at the next heartbeat.
        let record = Synced { etag: None, users: vec![], at: Instant::now() };
        synced().lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(peer_id.to_string(), record);
        return;
    };
    let (Ok(users), Ok(profiles)) = (UserStore::open_default(), ProfileStore::open_default()) else {
        return;
    };
    let linked = linked_users(&users, peer_id);
    let fetch = |path: &str| super::client::call(store, &peer, "GET", path, None);
    let result = if linked.is_empty() { Ok(vec![]) } else { sync_with(store, &users, &profiles, peer_id, &fetch) };
    let record = match result {
        Ok(changed) => {
            for user in changed {
                crate::management::v2::documents::publish_host_event(
                    USERS_CHANGED,
                    json!({ "userId": user, "reason": "peerProfile", "peerId": peer_id }),
                );
            }
            Synced { etag: hello, users: linked, at: Instant::now() }
        }
        // Retry at the next heartbeat (refreshed), not every worker tick.
        Err(_) => Synced { etag: None, users: vec![], at: Instant::now() },
    };
    synced().lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(peer_id.to_string(), record);
}

/// Forget a removed peer's memo.
pub fn forget(peer_id: &str) {
    synced().lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(peer_id);
    hello_etags().lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(peer_id);
}

/// `profileSource` of a `GET /v2/users` item.
pub fn source_view(profile: Option<&user_profiles::Profile>) -> Value {
    profile.map_or(Value::Null, user_profiles::Profile::source_view)
}

#[cfg(test)]
#[path = "profiles_tests.rs"]
mod tests;
