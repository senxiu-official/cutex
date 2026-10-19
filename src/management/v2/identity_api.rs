//! Person identity claims (capability `person-identity`; cutex-desktop
//! docs/research/person-identity-p2-20261014.md §3). Any authenticated
//! principal, pending included; a claim only ever concerns the caller's own
//! credential.
//!
//! - `GET  /v2/identity/challenge` → `{nonce, hostId, tokenId, expiresAt}`
//!   (`tokenId` = the calling client token, `"root"` for the root
//!   credential; one use, 30 s; 429 `too_many_challenges`)
//! - `POST /v2/identity/claim` `{publicKey, signature, nonce?, displayName?}`
//!   → `{outcome, tokenId, userId, pending, personUserId, publicKey,
//!   fingerprint}`; the signature (Ed25519, base64 / base64url) covers
//!   `"cutex-identity-v1\n" + hostId + "\n" + nonce + "\n" + tokenId`.
//!   Without `nonce` every live nonce of the caller is tried. Every nonce a
//!   claim names is spent, valid signature or not.

use std::net::TcpStream;
use std::time::Instant;

use serde_json::{json, Value};

use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::management::persons::{self, ChallengeError, Challenges, VerifiedClaim, ROOT_TOKEN_ID};
use crate::management::users::{self, UserError, UserStore};

pub const CHALLENGE_PATH: &str = "/v2/identity/challenge";
pub const CLAIM_PATH: &str = "/v2/identity/claim";

pub fn is_path(path: &str) -> bool {
    path == CHALLENGE_PATH || path == CLAIM_PATH
}

fn host_id() -> String {
    crate::platform::host::current_host_name()
}

/// The calling credential: a client / device token id, or `None` = root.
fn caller() -> Option<String> {
    crate::management::listeners::current_client_token().map(|token| token.token_id)
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let result = UserStore::open_default()
        .map_err(|e| UserError::new(503, "users_unavailable", format!("{e:#}")))
        .and_then(|store| route(&store, Challenges::global(), caller().as_deref(), request));
    match result {
        Ok(value) => write_json_response(stream, 200, "OK", &value),
        Err(failure) => {
            let reason = match failure.status {
                400 => "Bad Request",
                403 => "Forbidden",
                404 => "Not Found",
                405 => "Method Not Allowed",
                429 => "Too Many Requests",
                500 => "Internal Server Error",
                _ => "Service Unavailable",
            };
            let retryable = matches!(failure.status, 429 | 503);
            super::server::write_v2_error(stream, failure.status, reason, failure.code, &failure.message, retryable, failure.details)
        }
    }
}

pub(crate) fn route(
    store: &UserStore,
    challenges: &Challenges,
    token_id: Option<&str>,
    request: &SimpleHttpRequest,
) -> Result<Value, UserError> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let signer = token_id.unwrap_or(ROOT_TOKEN_ID);
    match (request.method.as_str(), path) {
        ("GET", CHALLENGE_PATH) => {
            let now = Instant::now();
            let nonce = challenges.issue(signer, now).map_err(|error| match error {
                ChallengeError::TooMany => UserError::new(429, "too_many_challenges", "too many identity challenges are outstanding; retry shortly"),
                ChallengeError::Unavailable => UserError::new(503, "identity_unavailable", "no secure random source"),
            })?;
            let expires = chrono::Utc::now() + chrono::Duration::from_std(persons::CHALLENGE_TTL).unwrap_or_default();
            Ok(json!({
                "nonce": nonce,
                "hostId": host_id(),
                "tokenId": signer,
                "expiresAt": expires.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            }))
        }
        ("POST", CLAIM_PATH) => claim(store, challenges, token_id, request),
        (_, CHALLENGE_PATH | CLAIM_PATH) => {
            Err(UserError::new(405, "method_not_allowed", "unsupported method for this route"))
        }
        _ => Err(UserError::new(404, "route_not_found", "no such identity route")),
    }
}

fn claim(
    store: &UserStore,
    challenges: &Challenges,
    token_id: Option<&str>,
    request: &SimpleHttpRequest,
) -> Result<Value, UserError> {
    let object = match serde_json::from_slice::<Value>(&request.body) {
        Ok(Value::Object(object)) => object,
        _ => return Err(UserError::invalid("a JSON object body is required")),
    };
    if let Some(key) = object.keys().find(|k| !matches!(k.as_str(), "publicKey" | "signature" | "nonce" | "displayName")) {
        return Err(UserError::invalid(format!("unsupported field: {key}")));
    }
    let text = |key: &str| -> Result<Option<&str>, UserError> {
        match object.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) => Ok(Some(value.as_str())),
            Some(_) => Err(UserError::invalid(format!("{key} must be a string"))),
        }
    };
    let public_key = text("publicKey")?.ok_or_else(|| UserError::invalid("publicKey is required"))?;
    let signature = text("signature")?.ok_or_else(|| UserError::invalid("signature is required"))?;
    let nonce = text("nonce")?;
    let display_name = text("displayName")?
        .map(crate::management::user_profiles::validate_display_name)
        .transpose()?;
    let signer = token_id.unwrap_or(ROOT_TOKEN_ID);
    // Spend the named nonce (or all of the caller's) before anything can fail.
    let candidates = challenges.take(signer, nonce, Instant::now());
    let public_key = persons::normalize_public_key(public_key)
        .ok_or_else(|| UserError::new(400, "invalid_public_key", "publicKey must be base64 / base64url of a 32-byte Ed25519 key"))?;
    if candidates.is_empty() {
        return Err(UserError::new(400, "challenge_invalid", "no live identity challenge for this credential; GET /v2/identity/challenge first"));
    }
    let host = host_id();
    let verified = candidates.iter().any(|nonce| {
        persons::verify_signature(&public_key, persons::signing_message(&host, nonce, signer).as_bytes(), signature)
    });
    if !verified {
        users::audit(store.dir(), "personClaimRejected", json!({ "tokenId": signer, "publicKey": public_key }));
        return Err(UserError::new(400, "invalid_signature", "the signature does not verify for this key and challenge"));
    }
    let result = store.apply_claim(
        &VerifiedClaim { token_id: token_id.map(str::to_string), public_key: public_key.clone(), display_name },
        &users::current_actor(),
    )?;
    // owner-device-notice: a pending device auto-bound to the owner.
    users::announce_owner_devices(store);
    Ok(json!({
        "outcome": result.outcome.as_str(),
        "tokenId": result.token_id,
        "userId": result.user_id,
        "pending": result.user_id.is_none(),
        "personUserId": result.person_user_id,
        "publicKey": public_key,
        "fingerprint": persons::fingerprint(&public_key),
    }))
}
