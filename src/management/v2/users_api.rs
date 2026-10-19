//! Users and grants (capability `users`; owner only — the access gate
//! refuses every non-owner principal before this module runs):
//!
//! - `GET    /v2/users` → `{revision, users[], tokens[], peers[], settings}`
//!   (tokens carry `userId` / `pending`; never secrets or hashes)
//! - `POST   /v2/users` `{name, color?, grant?}` → 201 `{user, revision}`
//! - `PATCH  /v2/users/{id}` `{name?, color?, grant?}` → `{user, revision}`
//! - `DELETE /v2/users/{id}` → `{userId, deleted}`; 409 `user_in_use`
//! - `PUT    /v2/users/token-bindings/{tokenId}` `{userId|null}`
//! - `PUT    /v2/users/peer-bindings/{peerId}` `{userId}`
//! - `PATCH  /v2/users/settings` `{ownerRequiresApproval}`; turning approval
//!   off (`false`) is a root-identity action (owner-local-only: the local or
//!   an SSH-tunnelled desktop, see `users::request_is_owner_local`), else 403 `owner_local_only`
//!   `{action: "disableOwnerApproval"}`. Turning it on stays open to every
//!   owner principal.
//!
//! New owner devices (capability `owner-device-notice`; cutex-desktop
//! docs/research/host-ownership-20261019.md §5 limit 1; owner principals):
//!
//! - `GET  /v2/users/owner/new-devices` → `{devices: [<GET /v2/users tokens[]
//!   item> + {at, via, current}]}`: tokens bound to the owner (pairing,
//!   assignment, person-key auto-binding, the CLI = `ssh`) that no owner
//!   principal acknowledged yet. Host event `cutex/access/ownerDeviceAdded
//!   {tokenId, label, deviceName, platform, scope, at, via}`.
//! - `POST /v2/users/owner/new-devices/{tokenId}/ack` → `{tokenId,
//!   acknowledged}` (`false` = nothing to acknowledge; idempotent).
//!
//! Self service (capability `users-self-profile`; any assigned principal —
//! owner or a bound user, never pending — for its own record only):
//!
//! - `GET    /v2/users/me` → `{user: {id, name, displayName, avatarEtag, isOwner}}`
//! - `PATCH  /v2/users/me` `{displayName?: string|null, avatar?: {contentType, data}|null}`
//!   → the same (`data` = base64 of a square PNG / JPEG / WebP ≤ 48 KiB)
//! - `GET    /v2/users/{id}/avatar` → the image (ETag, `If-None-Match` → 304);
//!   the owner sees every user, a non-owner the owner, itself and the users
//!   that can see at least one agent it sees (404 otherwise)
//!
//! Own devices (capability `user-own-devices`; owner, root or a bound user —
//! a pending device gets 403 `user_pending`):
//!
//! - `GET    /v2/users/me/devices` → `{hostId, userId, devices[]}`: the client
//!   tokens bound to the caller's user (root / owner-bound tokens: the owner
//!   user), each a `GET /v2/users` `tokens[]` item plus `current` (the token
//!   of this request)
//! - `DELETE /v2/users/me/devices/{tokenId}` → `{tokenId, revoked: true,
//!   current}`; a token not bound to the caller (or unknown): 404
//!   `client_token_not_found`. Revoking the current token signs this device
//!   out (later requests with it get 401).
//!
//! Home page (capability `user-home`, `crate::management::user_home`; any
//! assigned principal for its own record — owner or a bound user, never
//! pending; device scope `read` may only read):
//!
//! - `GET    /v2/users/me/home` → `{revision, home: {...}|null, coverEtag,
//!   updatedAt}` (`home.coverEtag` mirrors `coverEtag`)
//! - `PUT    /v2/users/me/home` `{revision, home: {...}|null}` → the same;
//!   409 `home_revision_conflict` `{revision}` when `revision` is not the
//!   current one; 400 `invalid_home`, 413 `home_too_large`
//! - `GET    /v2/users/me/home/cover` → the image (ETag, `If-None-Match` →
//!   304; 404 `cover_not_found`)
//! - `PUT    /v2/users/me/home/cover` `{contentType, data}` (base64 of a PNG /
//!   JPEG / WebP ≤ 2 MiB, each side ≤ 4096 px) → the `GET` home answer;
//!   400 `invalid_cover`, 413 `cover_too_large`
//! - `DELETE /v2/users/me/home/cover` → the same with `coverEtag: null`
//!
//! `GET /v2/capabilities` `user.token` (capability `user-own-token`): the
//! token of the request `{id, label, deviceName, ownerName, platform}`, null
//! for the root credential.
//!
//! Listings (`GET /v2/users` items, `/v2/capabilities` `user`, `actingUser`)
//! carry `displayName` and `avatarEtag`.
//!
//! Peer owner profiles (capability `peer-owner-profile`, `crate::peers::profiles`):
//! a user bound to a paired peer owner's person key gets that owner's name /
//! avatar unless set or cleared here directly; `GET /v2/users` items carry
//! `profileSource` (`null` = all direct, else `{kind: "peer", peerId,
//! fields: ["displayName"|"avatar"], updatedAt}`); host event
//! `cutex/users/changed` `{userId, reason: "peerProfile", peerId}`.
//!
//! Person identity (capability `person-identity`; owner only):
//!
//! - `GET /v2/users` gains `personsRevision`, `tokenPersons`
//!   `{tokenId: publicKey}`, `personKeys[]` on every user, `person` on every
//!   token (`{publicKey, fingerprint, userId, displayName, claimedAt,
//!   previousPublicKey}` or null) and `personKeys` (public keys) on peers.
//! - `PUT /v2/users/{id}/person-keys` `{personKeys: [{publicKey, label?}]}`
//!   replaces the user's keys → `{userId, personKeys[]}`; 409
//!   `person_key_in_use` when another user holds a key.
//! - `DELETE /v2/users/token-persons/{tokenId}/previous` (capability
//!   `person-identity-ack`): the owner acknowledges that a device presented
//!   another key; clears that token's `person.previousPublicKey` →
//!   `{tokenId, cleared, previousPublicKey: null, personsRevision}`
//!   (`cleared` = a key was set; idempotent). 404 `client_token_not_found`.
//!   The device's current key and its binding do not change.
//!
//! Owner person (capability `host-owner-person`; cutex-desktop
//! docs/research/host-ownership-20261019.md §1):
//!
//! - `PUT /v2/users/owner-person` `{publicKey}` → `{owner}` (the `GET
//!   /v2/host` `owner` object): the key must already be one of owner's
//!   person keys (400 `not_an_owner_key`); source `local`.
//! - `DELETE /v2/users/owner-person` → `{owner}` (unconfirmed).
//! - Both only owner-local (`users::request_is_owner_local`: a loopback
//!   listener with the root credential or an owner token issued by
//!   `client-token issue` on the host, i.e. the local or an SSH-tunnelled
//!   desktop): a paired device's token (owner-bound included) and any remote
//!   listener get 403 `owner_person_local_only` `{clientToken, issuedVia}`.

use std::net::TcpStream;

use serde_json::{json, Value};

use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::management::client_tokens::{ClientTokenStore, ClientTokenSummary, TOKEN_ID_PREFIX};
use crate::management::persons;
use crate::management::user_home::{self, HomeStore};
use crate::management::user_profiles::{self, ProfilePatch, ProfileStore};
use crate::management::users::{self, Grant, Principal, UserError, UserStore};

pub const PATH: &str = "/v2/users";
const TOKEN_BINDINGS: &str = "/v2/users/token-bindings/";
const PEER_BINDINGS: &str = "/v2/users/peer-bindings/";
const SETTINGS: &str = "/v2/users/settings";
pub const ME: &str = "/v2/users/me";
pub const ME_DEVICES: &str = "/v2/users/me/devices";
const ME_DEVICES_PREFIX: &str = "/v2/users/me/devices/";
pub const ME_HOME: &str = "/v2/users/me/home";
pub const ME_HOME_COVER: &str = "/v2/users/me/home/cover";
const AVATAR_SUFFIX: &str = "/avatar";
const PERSON_KEYS_SUFFIX: &str = "/person-keys";
const TOKEN_PERSONS: &str = "/v2/users/token-persons/";
const PREVIOUS_SUFFIX: &str = "/previous";
pub const OWNER_PERSON: &str = "/v2/users/owner-person";
pub const NEW_OWNER_DEVICES: &str = "/v2/users/owner/new-devices";
const NEW_OWNER_DEVICES_PREFIX: &str = "/v2/users/owner/new-devices/";
const ACK_SUFFIX: &str = "/ack";

pub fn is_path(path: &str) -> bool {
    path == PATH || path.starts_with("/v2/users/")
}

/// `GET|PATCH /v2/users/me`, `GET /v2/users/{id}/avatar`, own devices and
/// the home page: the routes a non-owner may use (the access gate classifies
/// them).
pub fn is_self_service_path(path: &str) -> bool {
    path == ME
        || avatar_user(path).is_some()
        || path == ME_DEVICES
        || me_device_id(path).is_some()
        || is_home_path(path)
}

/// `/v2/users/me/home` and `/v2/users/me/home/cover` (user-home).
pub fn is_home_path(path: &str) -> bool {
    path == ME_HOME || path == ME_HOME_COVER
}

/// `{tokenId}` of `/v2/users/me/devices/{tokenId}` (user-own-devices).
pub fn me_device_id(path: &str) -> Option<&str> {
    path.strip_prefix(ME_DEVICES_PREFIX).filter(|id| {
        id.starts_with(TOKEN_ID_PREFIX)
            && id.len() <= 64
            && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

/// `user.token` of `GET /v2/capabilities` (user-own-token): `{id, label,
/// deviceName, ownerName, platform, scope, issuedVia}` of a client / device token; `None` when
/// the store cannot be read or the token no longer exists.
pub fn own_token_view(token_id: &str) -> Option<Value> {
    let tokens = ClientTokenStore::open_default().and_then(|store| store.list()).ok()?;
    let token = tokens.into_iter().find(|token| token.token_id == token_id)?;
    Some(json!({
        "id": token.token_id,
        "label": token.label,
        "deviceName": token.device_name,
        "ownerName": token.owner_name,
        "platform": token.platform,
        "scope": token.scope,
        "issuedVia": token.issued_via,
    }))
}

/// `{id}` of `/v2/users/{id}/avatar`.
pub fn avatar_user(path: &str) -> Option<&str> {
    path.strip_prefix("/v2/users/")?
        .strip_suffix(AVATAR_SUFFIX)
        .filter(|id| users::valid_user_id(id))
}

fn error(stream: &mut TcpStream, error: UserError) -> anyhow::Result<()> {
    let reason = match error.status {
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        _ => "Service Unavailable",
    };
    super::server::write_v2_error(stream, error.status, reason, error.code, &error.message, error.status == 503, error.details)
}

fn body(request: &SimpleHttpRequest) -> Result<serde_json::Map<String, Value>, UserError> {
    if request.body.iter().all(u8::is_ascii_whitespace) {
        return Ok(Default::default());
    }
    match serde_json::from_slice::<Value>(&request.body) {
        Ok(Value::Object(object)) => Ok(object),
        _ => Err(UserError::invalid("a JSON object body is required")),
    }
}

fn only(object: &serde_json::Map<String, Value>, allowed: &[&str]) -> Result<(), UserError> {
    match object.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(key) => Err(UserError::invalid(format!("unsupported field: {key}"))),
        None => Ok(()),
    }
}

fn grant_field(object: &serde_json::Map<String, Value>) -> Result<Option<Grant>, UserError> {
    object
        .get("grant")
        .map(|value| serde_json::from_value::<Grant>(value.clone()).map_err(|e| UserError::invalid(format!("grant: {e}"))))
        .transpose()
}

fn color_field(object: &serde_json::Map<String, Value>) -> Result<Option<Option<String>>, UserError> {
    match object.get("color") {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::String(color)) => Ok(Some(Some(color.clone()))),
        Some(_) => Err(UserError::invalid("color must be #rrggbb or null")),
    }
}

fn peers_view() -> Vec<Value> {
    peers_view_with(&persons::State::default())
}

fn peers_view_with(persons: &persons::State) -> Vec<Value> {
    crate::peers::store::Store::open()
        .and_then(|store| store.snapshot())
        .map(|state| {
            state
                .peers
                .values()
                .map(|peer| json!({
                    "peerId": peer.peer_id,
                    "label": peer.label,
                    "hostId": peer.host_id,
                    "status": peer.status,
                    "userId": peer.user_id,
                    "personKeys": persons.peer_persons.get(&peer.peer_id).cloned().unwrap_or_default(),
                }))
                .collect()
        })
        .unwrap_or_default()
}

/// One `GET /v2/users` `tokens[]` item (also `GET /v2/users/me/devices`).
fn token_item(state: &users::State, persons: &persons::State, token: &ClientTokenSummary) -> Value {
    let user = state.user_of_token(&token.token_id).map(|user| user.id.clone());
    let mut value = serde_json::to_value(token).unwrap_or_default();
    value["pending"] = json!(user.is_none());
    value["userId"] = json!(user);
    value["person"] = persons.token_persons.get(&token.token_id).map_or(Value::Null, |key| {
        let claim = persons.token_claims.get(&token.token_id).cloned().unwrap_or_default();
        json!({
            "publicKey": key,
            "fingerprint": persons::fingerprint(key),
            "userId": persons.user_of_key(state, key),
            "displayName": claim.display_name,
            "claimedAt": if claim.claimed_at.is_empty() { Value::Null } else { json!(claim.claimed_at) },
            "previousPublicKey": claim.previous_public_key,
        })
    });
    value
}

pub fn listing(store: &UserStore) -> Result<Value, UserError> {
    let state = store.snapshot().map_err(|e| UserError::new(503, "users_unavailable", format!("{e:#}")))?;
    let tokens = store
        .tokens()
        .list()
        .map_err(|e| UserError::new(503, "client_tokens_unavailable", format!("{e:#}")))?;
    let profiles = ProfileStore::open_default()
        .and_then(|store| store.snapshot())
        .unwrap_or_default();
    // Person identity decorates only: a broken persons file never hides users.
    let persons = store.persons_snapshot().unwrap_or_else(|error| {
        eprintln!("persons store unavailable; listing without person keys: {error:#}");
        persons::State::default()
    });
    let token_ids: std::collections::HashSet<String> = tokens.iter().map(|t| t.token_id.clone()).collect();
    let tokens: Vec<Value> = tokens.iter().map(|token| token_item(&state, &persons, token)).collect();
    let token_persons: serde_json::Map<String, Value> = persons
        .token_persons
        .iter()
        .filter(|(token, _)| token_ids.contains(*token))
        .map(|(token, key)| (token.clone(), json!(key)))
        .collect();
    Ok(json!({
        "hostId": crate::platform::host::current_host_name(),
        "revision": state.revision,
        "users": state
            .users
            .iter()
            .map(|user| {
                let mut view = user.view();
                user_profiles::decorate(&mut view, profiles.profile(&user.id));
                view["profileSource"] = crate::peers::profiles::source_view(profiles.profile(&user.id));
                view["personKeys"] = persons.keys_of(&user.id).iter().map(persons::PersonKey::view).collect();
                view
            })
            .collect::<Vec<_>>(),
        "tokens": tokens,
        "tokenPersons": token_persons,
        "personsRevision": persons.revision,
        "peers": peers_view_with(&persons),
        "settings": state.settings,
    }))
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let store = match UserStore::open_default() {
        Ok(store) => store,
        Err(e) => return error(stream, UserError::new(503, "users_unavailable", format!("{e:#}"))),
    };
    match route(&store, request) {
        Ok((status, value)) => write_json_response(stream, status, if status == 201 { "Created" } else { "OK" }, &value),
        Err(failure) => error(stream, failure),
    }
}

pub(crate) fn route(store: &UserStore, request: &SimpleHttpRequest) -> Result<(u16, Value), UserError> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let actor = users::current_actor();
    let method = request.method.as_str();
    let revision = |store: &UserStore| store.snapshot().map(|s| s.revision).unwrap_or_default();
    if path == PATH {
        return match method {
            "GET" => Ok((200, listing(store)?)),
            "POST" => {
                let object = body(request)?;
                only(&object, &["name", "color", "grant"])?;
                let name = object.get("name").and_then(Value::as_str).ok_or_else(|| UserError::invalid("name is required"))?;
                let color = color_field(&object)?.flatten();
                let grant = grant_field(&object)?.unwrap_or_default();
                let user = store.create_user(name, color, grant, &actor)?;
                Ok((201, json!({ "user": user.view(), "revision": revision(store) })))
            }
            _ => Err(UserError::new(405, "method_not_allowed", "unsupported method for this route")),
        };
    }
    if path == SETTINGS {
        if method != "PATCH" {
            return Err(UserError::new(405, "method_not_allowed", "unsupported method for this route"));
        }
        let object = body(request)?;
        only(&object, &["ownerRequiresApproval"])?;
        let value = object
            .get("ownerRequiresApproval")
            .and_then(Value::as_bool)
            .ok_or_else(|| UserError::invalid("ownerRequiresApproval (boolean) is required"))?;
        if !value && !users::request_is_owner_local() {
            return Err(users::owner_local_only("disableOwnerApproval"));
        }
        let settings = store.set_settings(value, &actor)?;
        return Ok((200, json!({ "settings": settings })));
    }
    if path == OWNER_PERSON {
        return owner_person(store, request, method, &actor);
    }
    if path == NEW_OWNER_DEVICES {
        if method != "GET" {
            return Err(UserError::new(405, "method_not_allowed", "unsupported method for this route"));
        }
        return Ok((200, new_owner_devices(store)?));
    }
    if let Some(token_id) = path
        .strip_prefix(NEW_OWNER_DEVICES_PREFIX)
        .and_then(|rest| rest.strip_suffix(ACK_SUFFIX))
        .filter(|id| !id.is_empty() && !id.contains('/'))
    {
        if method != "POST" {
            return Err(UserError::new(405, "method_not_allowed", "unsupported method for this route"));
        }
        let acknowledged = store.ack_owner_device(token_id, &actor)?;
        return Ok((200, json!({ "tokenId": token_id, "acknowledged": acknowledged })));
    }
    if let Some(token_id) = path.strip_prefix(TOKEN_BINDINGS) {
        if method != "PUT" {
            return Err(UserError::new(405, "method_not_allowed", "unsupported method for this route"));
        }
        let object = body(request)?;
        only(&object, &["userId"])?;
        let user = match object.get("userId") {
            Some(Value::Null) => None,
            Some(Value::String(user)) => Some(user.as_str()),
            _ => return Err(UserError::invalid("userId (string or null) is required")),
        };
        store.bind_token(token_id, user, &actor)?;
        users::announce_owner_devices(store);
        return Ok((200, json!({ "tokenId": token_id, "userId": user, "pending": user.is_none() })));
    }
    if let Some(token_id) = path
        .strip_prefix(TOKEN_PERSONS)
        .and_then(|rest| rest.strip_suffix(PREVIOUS_SUFFIX))
        .filter(|id| !id.is_empty() && !id.contains('/'))
    {
        if method != "DELETE" {
            return Err(UserError::new(405, "method_not_allowed", "unsupported method for this route"));
        }
        let cleared = store.clear_previous_key(token_id, &actor)?;
        let persons_revision = store.persons_snapshot().map(|p| p.revision).unwrap_or_default();
        return Ok((200, json!({
            "tokenId": token_id,
            "cleared": cleared.is_some(),
            "previousPublicKey": Value::Null,
            "personsRevision": persons_revision,
        })));
    }
    if let Some(peer_id) = path.strip_prefix(PEER_BINDINGS) {
        if method != "PUT" {
            return Err(UserError::new(405, "method_not_allowed", "unsupported method for this route"));
        }
        let object = body(request)?;
        only(&object, &["userId"])?;
        let user = object.get("userId").and_then(Value::as_str).ok_or_else(|| UserError::invalid("userId is required"))?;
        bind_peer(store, peer_id, user, &actor)?;
        return Ok((200, json!({ "peerId": peer_id, "userId": user })));
    }
    if let Some(id) = path
        .strip_prefix("/v2/users/")
        .and_then(|rest| rest.strip_suffix(PERSON_KEYS_SUFFIX))
        .filter(|id| users::valid_user_id(id))
    {
        if method != "PUT" {
            return Err(UserError::new(405, "method_not_allowed", "unsupported method for this route"));
        }
        let object = body(request)?;
        only(&object, &["personKeys"])?;
        let items = object
            .get("personKeys")
            .and_then(Value::as_array)
            .ok_or_else(|| UserError::invalid("personKeys (array) is required"))?;
        let mut keys = Vec::new();
        for item in items {
            let item = item.as_object().ok_or_else(|| UserError::invalid("personKeys items must be {publicKey, label?}"))?;
            only(item, &["publicKey", "label"])?;
            let key = item.get("publicKey").and_then(Value::as_str).ok_or_else(|| UserError::invalid("publicKey is required"))?;
            let label = match item.get("label") {
                None | Some(Value::Null) => None,
                Some(Value::String(label)) => Some(label.clone()),
                Some(_) => return Err(UserError::invalid("label must be a string or null")),
            };
            keys.push((key.to_string(), label));
        }
        let keys = store.set_person_keys(id, keys, &actor)?;
        return Ok((200, json!({ "userId": id, "personKeys": keys.iter().map(persons::PersonKey::view).collect::<Vec<_>>() })));
    }
    let Some(id) = path.strip_prefix("/v2/users/").filter(|id| users::valid_user_id(id)) else {
        return Err(UserError::new(404, "route_not_found", "no such users route"));
    };
    match method {
        "PATCH" => {
            let object = body(request)?;
            only(&object, &["name", "color", "grant"])?;
            let name = match object.get("name") {
                None => None,
                Some(Value::String(name)) => Some(name.as_str()),
                Some(_) => return Err(UserError::invalid("name must be a string")),
            };
            let user = store.update_user(id, name, color_field(&object)?, grant_field(&object)?, &actor)?;
            Ok((200, json!({ "user": user.view(), "revision": revision(store) })))
        }
        "DELETE" => {
            let peers: Vec<String> = peers_view()
                .into_iter()
                .filter(|peer| peer["userId"] == id)
                .filter_map(|peer| peer["peerId"].as_str().map(str::to_string))
                .collect();
            store.delete_user(id, &peers, &actor)?;
            store.forget_person_user(id);
            if let Ok(profiles) = ProfileStore::open_default() {
                profiles.remove(id);
            }
            if let Ok(homes) = HomeStore::open_default() {
                homes.remove(id);
            }
            Ok((200, json!({ "userId": id, "deleted": true })))
        }
        _ => Err(UserError::new(405, "method_not_allowed", "unsupported method for this route")),
    }
}

/// `PUT|DELETE /v2/users/owner-person`: owner-local only (the local or an
/// SSH-tunnelled desktop; `users::request_is_owner_local`).
fn owner_person(store: &UserStore, request: &SimpleHttpRequest, method: &str, actor: &str) -> Result<(u16, Value), UserError> {
    if !matches!(method, "PUT" | "DELETE") {
        return Err(UserError::new(405, "method_not_allowed", "unsupported method for this route"));
    }
    if !users::request_is_owner_local() {
        let mut error = UserError::new(
            403,
            "owner_person_local_only",
            "the owner person can only be set on this host or over SSH: the root credential or an owner token issued with `cutex management client-token issue`, on a loopback listener, or `cutex management owner`",
        );
        error.details = users::owner_local_details(json!({}));
        return Err(error);
    }
    if method == "PUT" {
        let object = body(request)?;
        only(&object, &["publicKey"])?;
        let key = object.get("publicKey").and_then(Value::as_str).ok_or_else(|| UserError::invalid("publicKey is required"))?;
        store.set_owner_person(key, actor)?;
    } else {
        store.clear_owner_person(actor)?;
    }
    Ok((200, json!({ "owner": store.owner_view() })))
}

/// `GET /v2/users/owner/new-devices` (owner-device-notice).
fn new_owner_devices(store: &UserStore) -> Result<Value, UserError> {
    let state = store.snapshot().map_err(|e| UserError::new(503, "users_unavailable", format!("{e:#}")))?;
    let tokens = store
        .tokens()
        .list()
        .map_err(|e| UserError::new(503, "client_tokens_unavailable", format!("{e:#}")))?;
    let persons = store.persons_snapshot().unwrap_or_default();
    let current = crate::management::listeners::current_client_token().map(|token| token.token_id);
    let devices: Vec<Value> = state
        .new_owner_devices
        .iter()
        .filter_map(|notice| {
            let token = tokens.iter().find(|token| token.token_id == notice.token_id)?;
            let mut item = token_item(&state, &persons, token);
            item["at"] = json!(notice.at);
            item["via"] = json!(notice.via);
            item["current"] = json!(current.as_deref() == Some(notice.token_id.as_str()));
            Some(item)
        })
        .collect();
    Ok(json!({ "devices": devices }))
}

fn bind_peer(store: &UserStore, peer_id: &str, user: &str, actor: &str) -> Result<(), UserError> {
    let state = store.snapshot().map_err(|e| UserError::new(503, "users_unavailable", format!("{e:#}")))?;
    if state.user(user).is_none() {
        return Err(UserError::new(404, "user_not_found", "no user with this id"));
    }
    let peers = crate::peers::store::Store::open()
        .map_err(|e| UserError::new(503, "peers_unavailable", e.code))?;
    peers
        .with(|state| {
            let peer = state
                .peers
                .get_mut(peer_id)
                .ok_or_else(|| crate::peers::Error::new("peer_not_found", 404))?;
            if peer.user_id != user {
                peer.user_id = user.to_string();
                peer.policy_revision += 1;
            }
            Ok(())
        })
        .map_err(|e| {
            let code = if e.code == "peer_not_found" { "peer_not_found" } else { "peers_unavailable" };
            UserError::new(e.status, code, e.code)
        })?;
    crate::peers::routes::changed(peer_id, "user");
    users::audit(store.dir(), "peerBound", json!({ "peerId": peer_id, "userId": user, "actor": actor }));
    Ok(())
}

// ------------------------------------------------------------ self service

pub fn handle_self_service(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let principal = users::current_principal();
    if is_home_path(path) {
        return handle_home(stream, request, principal.as_ref(), path);
    }
    let stores = UserStore::open_default().and_then(|users| Ok((users, ProfileStore::open_default()?)));
    let (users_store, profiles) = match stores {
        Ok(stores) => stores,
        Err(e) => return error(stream, UserError::new(503, "users_unavailable", format!("{e:#}"))),
    };
    if let Some(target) = avatar_user(path) {
        if request.method != "GET" {
            return error(stream, UserError::new(405, "method_not_allowed", "unsupported method for this route"));
        }
        return write_avatar(stream, request, &users_store, &profiles, principal.as_ref(), target);
    }
    if path == ME_DEVICES || me_device_id(path).is_some() {
        return match own_devices(&users_store, principal.as_ref(), request, me_device_id(path)) {
            Ok(value) => write_json_response(stream, 200, "OK", &value),
            Err(failure) => error(stream, failure),
        };
    }
    match self_profile(&users_store, &profiles, principal.as_ref(), request) {
        Ok(value) => write_json_response(stream, 200, "OK", &value),
        Err(failure) => error(stream, failure),
    }
}

/// The caller's own view: `{user: {id, name, displayName, avatarEtag, isOwner}}`.
fn me_view(users_store: &UserStore, profiles: &ProfileStore, me: &str) -> Result<Value, UserError> {
    let state = users_store.snapshot().map_err(|e| UserError::new(503, "users_unavailable", format!("{e:#}")))?;
    let user = state.user(me).ok_or_else(|| UserError::new(404, "user_not_found", "no user with this id"))?;
    let mut view = json!({ "id": user.id, "name": user.name, "isOwner": user.is_owner() });
    user_profiles::decorate(&mut view, Some(&profiles.profile(me)));
    Ok(json!({ "user": view }))
}

pub(crate) fn self_profile(
    users_store: &UserStore,
    profiles: &ProfileStore,
    principal: Option<&Principal>,
    request: &SimpleHttpRequest,
) -> Result<Value, UserError> {
    let Some(me) = user_profiles::me(principal) else {
        return Err(UserError::new(403, "access_pending", "this device is waiting for the host owner to assign it to a user"));
    };
    match request.method.as_str() {
        "GET" => me_view(users_store, profiles, &me),
        "PATCH" => {
            let object = body(request)?;
            only(&object, &["displayName", "avatar"])?;
            let mut patch = ProfilePatch::default();
            match object.get("displayName") {
                None => {}
                Some(Value::Null) => patch.display_name = Some(None),
                Some(Value::String(name)) => patch.display_name = Some(Some(name.clone())),
                Some(_) => return Err(UserError::invalid("displayName must be a string or null")),
            }
            match object.get("avatar") {
                None => {}
                Some(Value::Null) => patch.avatar = Some(None),
                Some(value) => patch.avatar = Some(Some(user_profiles::parse_upload(value)?)),
            }
            // The record must exist (a binding to a deleted user is pending,
            // so this only fails on a race with a delete).
            me_view(users_store, profiles, &me)?;
            profiles.update(&me, patch, &users::current_actor())?;
            me_view(users_store, profiles, &me)
        }
        _ => Err(UserError::new(405, "method_not_allowed", "unsupported method for this route")),
    }
}

/// `GET /v2/users/me/devices` and `DELETE /v2/users/me/devices/{tokenId}`
/// (user-own-devices): the client tokens bound to the caller's user. The
/// root credential and owner-bound tokens act for the owner user; a pending
/// device has no user (403 `user_pending`). A token bound to someone else
/// reads as unknown (404 `client_token_not_found`, no existence leak).
pub(crate) fn own_devices(
    users_store: &UserStore,
    principal: Option<&Principal>,
    request: &SimpleHttpRequest,
    token_id: Option<&str>,
) -> Result<Value, UserError> {
    let Some(me) = user_profiles::me(principal) else {
        return Err(UserError::new(403, "user_pending", "this device is waiting for the host owner to assign it to a user"));
    };
    let current = crate::management::listeners::current_client_token().map(|token| token.token_id);
    let state = users_store.snapshot().map_err(|e| UserError::new(503, "users_unavailable", format!("{e:#}")))?;
    let tokens = users_store
        .tokens()
        .list()
        .map_err(|e| UserError::new(503, "client_tokens_unavailable", format!("{e:#}")))?;
    let mine = |id: &str| state.user_of_token(id).is_some_and(|user| user.id == me);
    match (request.method.as_str(), token_id) {
        ("GET", None) => {
            let persons = users_store.persons_snapshot().unwrap_or_else(|error| {
                eprintln!("persons store unavailable; listing devices without person keys: {error:#}");
                persons::State::default()
            });
            let devices: Vec<Value> = tokens
                .iter()
                .filter(|token| mine(&token.token_id))
                .map(|token| {
                    let mut item = token_item(&state, &persons, token);
                    item["current"] = json!(current.as_deref() == Some(token.token_id.as_str()));
                    item
                })
                .collect();
            Ok(json!({
                "hostId": crate::platform::host::current_host_name(),
                "userId": me,
                "devices": devices,
            }))
        }
        ("DELETE", Some(token_id)) => {
            let not_found = || UserError::new(404, "client_token_not_found", "no such client token");
            if !tokens.iter().any(|token| token.token_id == token_id) || !mine(token_id) {
                return Err(not_found());
            }
            let revoked = users_store
                .tokens()
                .revoke(token_id)
                .map_err(|e| UserError::new(503, "client_tokens_unavailable", format!("{e:#}")))?;
            if !revoked {
                return Err(not_found());
            }
            users::audit(
                users_store.dir(),
                "ownDeviceRevoked",
                json!({ "tokenId": token_id, "userId": me, "actor": users::current_actor() }),
            );
            Ok(json!({
                "tokenId": token_id,
                "revoked": true,
                "current": current.as_deref() == Some(token_id),
            }))
        }
        _ => Err(UserError::new(405, "method_not_allowed", "unsupported method for this route")),
    }
}

/// `/v2/users/me/home[/cover]` (user-home): the caller's own home page.
fn handle_home(stream: &mut TcpStream, request: &SimpleHttpRequest, principal: Option<&Principal>, path: &str) -> anyhow::Result<()> {
    let homes = match HomeStore::open_default() {
        Ok(homes) => homes,
        Err(e) => return error(stream, UserError::new(503, "users_unavailable", format!("{e:#}"))),
    };
    let Some(me) = user_profiles::me(principal) else {
        return error(stream, UserError::new(403, "access_pending", "this device is waiting for the host owner to assign it to a user"));
    };
    if path == ME_HOME_COVER && request.method == "GET" {
        return write_cover(stream, request, &homes, &me);
    }
    match own_home(&homes, &me, request, path == ME_HOME_COVER) {
        Ok(value) => write_json_response(stream, 200, "OK", &value),
        Err(failure) => error(stream, failure),
    }
}

/// `GET|PUT /v2/users/me/home` and `PUT|DELETE /v2/users/me/home/cover` for
/// user `me`; answers the home view.
pub(crate) fn own_home(homes: &HomeStore, me: &str, request: &SimpleHttpRequest, cover: bool) -> Result<Value, UserError> {
    let actor = users::current_actor();
    let method_not_allowed = || UserError::new(405, "method_not_allowed", "unsupported method for this route");
    let record = match (cover, request.method.as_str()) {
        (false, "GET") => homes.get(me)?,
        (false, "PUT") => {
            let object = body(request)?;
            only(&object, &["revision", "home"])?;
            let revision = object
                .get("revision")
                .and_then(Value::as_u64)
                .ok_or_else(|| UserError::invalid("revision (a non-negative integer, 0 for no home page yet) is required"))?;
            let home = match object.get("home") {
                Some(home) => user_home::validate_home(home)?,
                None => return Err(UserError::invalid("home (an object or null) is required")),
            };
            homes.put(me, revision, home, &actor)?
        }
        (true, "PUT") => {
            let value: Value = serde_json::from_slice(&request.body)
                .map_err(|_| UserError::new(400, "invalid_cover", "JSON object body {contentType, data} required"))?;
            let upload = user_home::parse_cover_upload(&value)?;
            homes.set_cover(me, Some(&upload), &actor)?
        }
        (true, "DELETE") => homes.set_cover(me, None, &actor)?,
        _ => return Err(method_not_allowed()),
    };
    Ok(record.view())
}

fn write_cover(stream: &mut TcpStream, request: &SimpleHttpRequest, homes: &HomeStore, me: &str) -> anyhow::Result<()> {
    let Some((cover, bytes)) = homes.cover(me) else {
        return error(stream, UserError::new(404, "cover_not_found", "no home page cover for this user"));
    };
    write_image(stream, request, &cover, &bytes)
}

/// An image answer with its ETag (`If-None-Match` → 304).
fn write_image(stream: &mut TcpStream, request: &SimpleHttpRequest, image: &user_profiles::Avatar, bytes: &[u8]) -> anyhow::Result<()> {
    let etag = format!("\"{}\"", image.etag);
    let matches = request
        .headers
        .get("if-none-match")
        .is_some_and(|value| value.split(',').any(|tag| tag.trim().trim_start_matches("W/") == etag));
    let previous = crate::http::server::extra_response_headers();
    crate::http::server::set_extra_response_headers(format!(
        "{previous}ETag: {etag}\r\nCache-Control: private, no-cache\r\nX-Content-Type-Options: nosniff\r\n"
    ));
    let result = if matches {
        crate::http::server::write_http_response(stream, 304, "Not Modified", &image.content_type, &[])
    } else {
        crate::http::server::write_http_response(stream, 200, "OK", &image.content_type, bytes)
    };
    crate::http::server::set_extra_response_headers(previous);
    result
}

/// Whether `principal` may see `target`'s profile: the owner sees everyone;
/// a user sees the owner, itself and every user that can see at least one
/// agent it sees (it may meet them as "sent by" in a shared agent).
pub(crate) fn may_see_user(state: &users::State, principal: Option<&Principal>, target: &str) -> bool {
    match principal {
        None | Some(Principal::Owner) => state.user(target).is_some(),
        Some(Principal::Pending) => false,
        Some(Principal::User { id, grant, .. }) => {
            if target == users::OWNER || target == id {
                return true;
            }
            let Some(other) = state.user(target).and_then(|user| user.grant.clone()) else {
                return false;
            };
            let mine = users::Scope::cached(grant);
            let theirs = users::Scope::cached(&other);
            mine.sessions().iter().any(|session| theirs.sees_session(session))
        }
    }
}

fn write_avatar(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    users_store: &UserStore,
    profiles: &ProfileStore,
    principal: Option<&Principal>,
    target: &str,
) -> anyhow::Result<()> {
    let not_found = |stream: &mut TcpStream| {
        error(stream, UserError::new(404, "avatar_not_found", "no avatar for this user"))
    };
    let visible = users_store
        .snapshot()
        .map(|state| may_see_user(&state, principal, target))
        .unwrap_or(false);
    if !visible {
        return not_found(stream);
    }
    let Some((avatar, bytes)) = profiles.avatar(target) else {
        return not_found(stream);
    };
    write_image(stream, request, &avatar, &bytes)
}
