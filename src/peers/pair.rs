use super::{
    client,
    policy::Policy,
    store::{Peer, Store},
    token, Error, Result,
};
use crate::management::v2::pairing::{PairError, PairingState};
use serde_json::{json, Value};
use std::{net::IpAddr, sync::OnceLock, time::Instant};

pub const FEATURES: [&str; 4] = [
    "asks.v1",
    "asks.snapshot.v1",
    "asks.close.v1",
    "agents.list.v1",
];

/// Advertised and negotiated like [`FEATURES`], but never required by
/// [`validate_hello`]: older peers keep working without them.
pub const OPTIONAL_FEATURES: [&str; 1] = [super::tells::FEATURE];

pub fn features() -> Vec<String> {
    let all = FEATURES.iter().chain(OPTIONAL_FEATURES.iter());
    #[cfg(debug_assertions)]
    if crate::config::instance::is_isolated()
        && std::env::var_os("CUTEX_TEST_FORBID_REAL_HOME")
            .is_some_and(|value| !value.is_empty() && value != "0")
    {
        if let Ok(value) = std::env::var("CUTEX_PEER_TEST_FEATURES") {
            return all
                .filter(|feature| value.split(',').any(|part| part.trim() == **feature))
                .map(|feature| (*feature).into())
                .collect();
        }
    }
    all.map(|feature| (*feature).into()).collect()
}

fn codes() -> &'static PairingState {
    static CODES: OnceLock<PairingState> = OnceLock::new();
    CODES.get_or_init(PairingState::new)
}
pub fn host() -> String {
    crate::platform::host::current_host_name()
}
pub fn label() -> String {
    let value: String = host()
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(32)
        .collect();
    if value.is_empty() {
        "host".into()
    } else {
        value
    }
}
pub fn local_url() -> String {
    let config = crate::config::store::load_codez_config();
    format!(
        "http://127.0.0.1:{}",
        crate::management::service::management_port(&config)
    )
}
fn advertised_urls() -> Vec<String> {
    let mut urls = vec![local_url()];
    urls.extend(
        crate::management::listeners::pairing_urls()
            .into_iter()
            .filter(|u| client::addresses(u).is_ok()),
    );
    urls
}
fn callback_url(destination: &str) -> Result<String> {
    if client::addresses(destination)?
        .1
        .iter()
        .all(|a| a.ip().is_loopback())
    {
        return Ok(local_url());
    }
    advertised_urls()
        .into_iter()
        .find(|u| client::addresses(u).is_ok_and(|(_, a)| a.iter().all(|a| !a.ip().is_loopback())))
        .ok_or_else(|| Error::new("peer_callback_unavailable", 409))
}
pub fn hello() -> Value {
    json!({"instanceId":super::instance_id(),"hostId":host(),"label":label(),"platform":std::env::consts::OS,"protocol":1,"features":features(),"cutexBuild":crate::build_info::build_json()})
}
pub fn invite(body: &Value) -> Result<Value> {
    requested_owner_user(body, "targetUserId")?;
    let local_user = local_user(body)?;
    let name = body.get("label").and_then(Value::as_str);
    if name.is_some_and(|s| !super::valid_label(s)) {
        return Err(Error::new("invalid_request", 400));
    }
    // An explicit `userId` is the owner's choice and always wins; without
    // one the peer defaults to owner, which a known person key may narrow
    // at pairing (person identity, see `person_binding`).
    let explicit = body.get("userId").is_some().then(|| local_user.clone());
    let issued = codes()
        .issue_for(name.map(str::to_string), explicit, Instant::now())
        .ok_or_else(|| Error::new("peer_rate_limited", 429))?;
    Ok(
        json!({"code":issued.code,"expiresAt":issued.expires_at,"instanceId":super::instance_id(),"hostId":host(),"urls":advertised_urls(),"userId":local_user}),
    )
}
pub fn field<'a>(body: &'a Value, key: &str) -> Result<&'a str> {
    body.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::new("invalid_request", 400))
}

/// User selectors are strict: omission means owner; a malformed id is
/// refused and never silently becomes owner. Only the shape is checked here:
/// a selector sent by the remote side (`targetUserId`) is informational and
/// never decides a local binding.
pub(super) fn requested_owner_user<'a>(body: &'a Value, key: &str) -> Result<&'a str> {
    match body.get(key) {
        None => Ok(super::store::OWNER_USER_ID),
        Some(Value::String(user)) if crate::management::users::valid_user_id(user) => Ok(user),
        Some(Value::String(_)) => Err(Error::new("peer_user_unsupported", 400)),
        Some(_) => Err(Error::new("invalid_request", 400)),
    }
}

/// The local binding chosen by the owner (`userId`): it must exist here.
fn local_user(body: &Value) -> Result<String> {
    let user = requested_owner_user(body, "userId")?;
    if user != super::store::OWNER_USER_ID && !local_user_exists(user) {
        return Err(Error::new("user_not_found", 404));
    }
    Ok(user.to_string())
}

/// Person identity (P2 §4): the local binding of a new peer whose owner
/// did not pick a user explicitly. Default = owner; when one of the remote
/// owner's person keys belongs to a local user, the peer is bound to that
/// user instead. Never more than owner, and an explicit choice is kept.
fn person_binding(explicit: Option<String>, remote: &Value) -> (String, Vec<String>) {
    let keys = crate::management::persons::peer_keys(remote);
    let user = explicit.unwrap_or_else(|| {
        crate::management::persons::user_for_peer_keys(&keys)
            .unwrap_or_else(|| super::store::OWNER_USER_ID.into())
    });
    (user, keys)
}

/// After a peer was stored: remember the remote owner's keys and audit an
/// automatic binding.
fn record_person_binding(peer_id: &str, user: &str, keys: &[String], auto: bool) {
    let Ok(users) = crate::management::users::UserStore::open_default() else {
        return;
    };
    users.record_peer_persons(peer_id, keys);
    if auto && user != super::store::OWNER_USER_ID {
        crate::management::users::audit(
            users.dir(),
            "peerAutoBound",
            json!({ "peerId": peer_id, "userId": user, "publicKeys": keys }),
        );
    }
}

fn local_user_exists(user: &str) -> bool {
    crate::management::users::UserStore::open_default()
        .and_then(|users| users.snapshot())
        .is_ok_and(|state| state.user(user).is_some())
}
fn make_peer(id: &str, host: &str, name: &str, url: &str, inbound: &str) -> Result<Peer> {
    if !super::valid_id(id) || !super::valid_label(name) || host.len() > 128 {
        return Err(Error::new("invalid_request", 400));
    }
    if id == super::instance_id() {
        return Err(Error::new("self_peer", 400));
    }
    client::addresses(url)?;
    Ok(Peer {
        peer_id: id.into(),
        user_id: super::store::OWNER_USER_ID.into(),
        label: name.into(),
        host_id: host.into(),
        platform: "unknown".into(),
        urls: vec![url.trim_end_matches('/').into()],
        allow_loopback: false,
        last_successful_url: None,
        enabled: true,
        inbound_token_sha256: token::hash(inbound),
        policy: Policy::default(),
        policy_revision: 1,
        protocol: 1,
        features: vec![],
        negotiated_features: vec![],
        created_at: super::now(),
        last_seen_at: None,
        last_error: None,
        status: "pending".into(),
        next_probe_at: None,
        probe_failures: 0,
        pending_until: Some((chrono::Utc::now() + chrono::Duration::minutes(2)).to_rfc3339()),
    })
}

fn validate_callback(url: &str, source: Option<IpAddr>) -> Result<bool> {
    let allow_loopback = source.is_some_and(|ip| ip.is_loopback());
    let (_, addrs) = client::addresses(url).map_err(|error| {
        if !allow_loopback && error.code == "peer_url_insecure" {
            Error::new("peer_callback_forbidden", 403)
        } else {
            error
        }
    })?;
    client::validate_callback_addresses(&addrs, allow_loopback)?;
    Ok(allow_loopback)
}

pub fn receive(store: &Store, body: &Value) -> Result<Value> {
    // The remote's selectors are shape-checked only; the binding comes from
    // the local invite (`POST /v2/peers/invites` `userId`).
    requested_owner_user(body, "targetUserId")?;
    requested_owner_user(body, "userId")?;
    let source = crate::management::listeners::current_connection().map(|c| c.peer.ip());
    let (invite_label, invite_user) = codes()
        .redeem_with_user(field(body, "code")?, source, Instant::now())
        .map_err(|e| match e {
            PairError::InvalidCode => Error::new("invalid_pairing_code", 400),
            PairError::TooManyAttempts { retry_after } | PairError::Locked { retry_after } => {
                let mut e = Error::new("peer_rate_limited", 429);
                e.details = json!({"retryAfterSecs":retry_after});
                e
            }
        })?;
    let allow_loopback = validate_callback(field(body, "url")?, source)?;
    let id = field(body, "instanceId")?;
    let outgoing = field(body, "tokenForYou")?;
    let inbound = token::generate();
    let mut peer = make_peer(
        id,
        field(body, "hostId")?,
        invite_label.as_deref().unwrap_or(field(body, "label")?),
        field(body, "url")?,
        &inbound,
    )?;
    peer.allow_loopback = allow_loopback;
    let auto = invite_user.is_none();
    let (user, keys) = person_binding(invite_user, body);
    peer.user_id = user.clone();
    let peer_id = peer.peer_id.clone();
    store.insert(peer, outgoing)?;
    record_person_binding(&peer_id, &user, &keys, auto);
    let mut response = hello();
    response["tokenForYou"] = json!(inbound);
    response["ownerPersonKeys"] = json!(crate::management::persons::local_owner_keys());
    Ok(response)
}
pub fn declared_features(value: &Value) -> Vec<String> {
    value
        .get("features")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|feature| {
            !feature.is_empty()
                && feature.len() <= 80
                && feature
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_'))
        })
        .take(64)
        .map(str::to_string)
        .collect()
}

pub fn negotiated_features(value: &Value) -> Vec<String> {
    let declared = declared_features(value);
    features()
        .into_iter()
        .filter(|feature| declared.contains(feature))
        .collect()
}

pub fn validate_hello(peer: &Peer, value: &Value) -> Result<()> {
    if value.get("instanceId").and_then(Value::as_str) != Some(&peer.peer_id) {
        return Err(Error::new("identity_changed", 409));
    }
    if value.get("protocol").and_then(Value::as_u64) != Some(1) {
        let mut error = Error::new("peer_unsupported", 409);
        error.details = json!({
            "supportedProtocol": 1,
            "remoteProtocol": value.get("protocol").and_then(Value::as_u64),
        });
        return Err(error);
    }
    let negotiated = negotiated_features(value);
    let missing: Vec<_> = FEATURES
        .iter()
        .filter(|feature| !negotiated.iter().any(|common| common == **feature))
        .copied()
        .collect();
    if !missing.is_empty() {
        let mut error = Error::new("peer_unsupported", 409);
        error.details = json!({"missingFeatures":missing});
        return Err(error);
    }
    Ok(())
}
pub fn confirm(store: &Store, peer: &Peer) -> Result<Value> {
    // Confirm retries for an active relationship are local and idempotent.
    // Repeating the callback could otherwise create reciprocal per-peer lane
    // waits when both sides confirm at the same time.
    if store.get(&peer.peer_id)?.pending_until.is_none() {
        return Ok(json!({"confirmed":true}));
    }
    let response = client::call(store, peer, "GET", "/peer/v1/hello", None)?;
    validate_hello(peer, &response)?;
    store.settings(&json!({"enabled":true}))?;
    store.with(|s| {
        let p = s
            .peers
            .get_mut(&peer.peer_id)
            .ok_or_else(|| Error::new("peer_not_found", 404))?;
        p.pending_until = None;
        p.features = declared_features(&response);
        p.negotiated_features = negotiated_features(&response);
        p.platform = response["platform"].as_str().unwrap_or("unknown").into();
        p.status = "online".into();
        p.last_error = None;
        p.last_seen_at = Some(super::now());
        Ok(())
    })?;
    super::routes::changed(&peer.peer_id, "paired");
    Ok(json!({"confirmed":true}))
}
pub fn add(store: &Store, body: &Value) -> Result<Value> {
    let target_user = requested_owner_user(body, "targetUserId")?;
    let local_user = local_user(body)?;
    let url = field(body, "url")?;
    let allow_loopback = client::addresses(url)?
        .1
        .iter()
        .all(|address| address.ip().is_loopback());
    let settings = store.snapshot()?.settings;
    if let Some(name) = body.get("label").and_then(Value::as_str) {
        if !super::valid_label(name) {
            return Err(Error::new("invalid_request", 400));
        }
        if store.snapshot()?.peers.values().any(|p| p.label == name) {
            return Err(Error::new("label_taken", 409));
        }
    }
    let callback = callback_url(url)?;
    let inbound = token::generate();
    let request = json!({"code":field(body,"code")?,"instanceId":super::instance_id(),"hostId":host(),"label":label(),"url":callback,"tokenForYou":inbound,"targetUserId":target_user,"ownerPersonKeys":crate::management::persons::local_owner_keys()});
    let response = client::request_with_settings(
        url,
        "POST",
        "/peer/v1/pair",
        None,
        Some(&request),
        &settings,
        allow_loopback,
    );
    super::audit::record(
        store,
        "outbound",
        None,
        "/peer/v1/pair",
        &response,
        request.to_string().len(),
        None,
    );
    let response = response?;
    let outgoing = field(&response, "tokenForYou")?;
    let mut peer = make_peer(
        field(&response, "instanceId")?,
        field(&response, "hostId")?,
        body.get("label")
            .and_then(Value::as_str)
            .unwrap_or(field(&response, "label")?),
        url,
        &inbound,
    )?;
    peer.allow_loopback = allow_loopback;
    let explicit_user = body.get("userId").is_some().then_some(local_user);
    let auto = explicit_user.is_none();
    let (user, person_keys) = person_binding(explicit_user, &response);
    peer.user_id = user.clone();
    peer.features = declared_features(&response);
    peer.negotiated_features = negotiated_features(&response);
    peer.platform = response
        .get("platform")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .into();
    let result = (|| {
        validate_hello(&peer, &response)?;
        store.insert(peer.clone(), outgoing)?;
        record_person_binding(&peer.peer_id, &user, &person_keys, auto);
        let hello = client::call(store, &peer, "GET", "/peer/v1/hello", None)?;
        validate_hello(&peer, &hello)?;
        client::call(store, &peer, "POST", "/peer/v1/confirm", Some(&json!({})))?;
        confirm(store, &peer)?;
        Ok(json!({"peer":store.get(&peer.peer_id)?.view()}))
    })();
    if result.is_err() {
        let abort = client::request_with_settings(
            url,
            "POST",
            "/peer/v1/abort",
            Some(outgoing),
            Some(&json!({})),
            &settings,
            allow_loopback,
        );
        super::audit::record(
            store,
            "outbound",
            Some(&peer),
            "/peer/v1/abort",
            &abort,
            2,
            None,
        );
        if abort
            .as_ref()
            .is_err_and(|error| error.code == "peer_already_active")
        {
            // The remote confirm may have committed even when its response was
            // lost. Active relationships must use the normal removal lifecycle.
            let unpair = client::request_with_settings(
                url,
                "POST",
                "/peer/v1/unpair",
                Some(outgoing),
                Some(&json!({})),
                &settings,
                allow_loopback,
            );
            super::audit::record(
                store,
                "outbound",
                Some(&peer),
                "/peer/v1/unpair",
                &unpair,
                2,
                None,
            );
        }
        // Never remove an older relationship when a duplicate insert failed.
        if store
            .get(&peer.peer_id)
            .is_ok_and(|p| p.inbound_token_sha256 == peer.inbound_token_sha256)
        {
            let _ = store.remove(&peer.peer_id);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_scope_is_bound_to_the_pair_connection() {
        let remote = Some("100.64.0.21".parse().unwrap());
        let local = Some("127.0.0.1".parse().unwrap());
        for url in [
            "http://127.0.0.1:26270",
            "http://[::1]:26270",
            "http://10.0.0.1:26270",
            "http://169.254.1.1:26270",
            "http://8.8.8.8:26270",
        ] {
            assert_eq!(
                validate_callback(url, remote).unwrap_err().code,
                "peer_callback_forbidden"
            );
        }
        assert!(validate_callback("http://127.0.0.1:26270", local).unwrap());
        assert!(!validate_callback("http://100.64.0.22:26270", remote).unwrap());
        assert!(!validate_callback("http://[fd7a:115c:a1e0::2]:26270", remote).unwrap());
        assert!(validate_callback("http://127.0.0.1:26270", None).is_err());
    }

    #[test]
    fn hello_requires_the_feature_intersection_and_reports_missing_features() {
        let peer = make_peer(
            &format!("ci-{}", uuid::Uuid::new_v4().simple()),
            "other-host",
            "other",
            "http://127.0.0.1:26370",
            &token::generate(),
        )
        .unwrap();
        let subset =
            json!({"instanceId":peer.peer_id,"protocol":1,"features":["asks.v1","future.v9"]});
        let error = validate_hello(&peer, &subset).unwrap_err();
        assert_eq!(error.code, "peer_unsupported");
        assert!(!error.retryable);
        assert_eq!(
            error.details["missingFeatures"],
            json!(["asks.snapshot.v1", "asks.close.v1", "agents.list.v1"])
        );
        assert_eq!(negotiated_features(&subset), vec!["asks.v1"]);
        let full =
            json!({"instanceId":peer.peer_id,"protocol":1,"features":FEATURES,"extra":"ignored"});
        validate_hello(&peer, &full).unwrap();
        assert_eq!(negotiated_features(&full).len(), FEATURES.len());
        let mismatch = json!({"instanceId":peer.peer_id,"protocol":2,"features":FEATURES});
        let error = validate_hello(&peer, &mismatch).unwrap_err();
        assert_eq!(error.details["remoteProtocol"], 2);
        assert_eq!(error.details["supportedProtocol"], 1);
    }
}
