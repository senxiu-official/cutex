//! Cross-host `agent_tell`: an FYI note to an exported agent on a paired host.
//!
//! Sending is synchronous (`POST /peer/v1/tells`), so a refusal writes nothing
//! locally. The receiver authenticates the peer, applies the same target
//! validation and policy as incoming asks (exports, `inbound.accept`), dedupes
//! by the sender's tell ID and enqueues an ordinary passive note on its local
//! Agent Bus (`agent_bus::peer_tell`). Receipts live in
//! `runtime/peers/tell-receipts.json`, created on first use; older binaries
//! ignore the file.
use super::{store::Peer, store::Store, Error, Result};
use crate::asks::{store::AskError, tools::ToolContext};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Optional hello feature: advertised, never required by `validate_hello`.
pub const FEATURE: &str = "tells.v1";
pub const ROUTE: &str = "/peer/v1/tells";
pub const TEXT_MAX: usize = 16 * 1024;
pub const UNSUPPORTED_DETAIL: &str = "peer host does not support cross-host tells (upgrade it)";
const RECEIPT_DAYS: i64 = 7;
const RECEIPTS_PER_PEER: usize = 2000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireTellRequesterV1 {
    pub cutex_session_id: String,
    #[serde(default)]
    pub name: Option<String>,
}

/// Unknown fields are ignored (additive evolution); semantics that an older
/// receiver must not drop silently need a new feature.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireTellV1 {
    #[serde(default = "one")]
    pub schema_version: u32,
    pub tell_id: String,
    pub requester: WireTellRequesterV1,
    pub target: String,
    pub text: String,
}
fn one() -> u32 {
    1
}

fn valid_tell_id(id: &str) -> bool {
    id.strip_prefix("ptl-")
        .is_some_and(|v| v.len() == 32 && v.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn single_line_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
}

impl WireTellV1 {
    fn digest(&self) -> String {
        crate::asks::sha256_hex(
            json!({"requester":{"cutexSessionId":self.requester.cutex_session_id,"name":self.requester.name},"target":self.target,"text":self.text})
                .to_string()
                .as_bytes(),
        )
    }
}

/// One local bus enqueue (injected for tests).
pub struct Delivery<'a> {
    pub target: &'a str,
    pub text: &'a str,
    pub facts: &'a crate::agent_bus::peer_tell::PeerTellFacts,
    pub message_id: &'a str,
}

/// Production delivery: the local Agent Bus send path.
pub fn bus_delivery(delivery: &Delivery<'_>) -> Result<String> {
    let config = crate::config::store::load_codez_config();
    let sent = crate::agent_bus::client::send_peer_tell(
        &config,
        delivery.target,
        delivery.text,
        delivery.facts,
        delivery.message_id,
    )
    .map_err(|_| Error::new("bus_unavailable", 503))?;
    if sent.get("ok").and_then(Value::as_bool) == Some(false) {
        return Err(Error::new("bus_unavailable", 503));
    }
    Ok(sent["id"]
        .as_str()
        .unwrap_or(delivery.message_id)
        .to_string())
}

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Receipts {
    #[serde(default)]
    peers: BTreeMap<String, BTreeMap<String, Receipt>>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Receipt {
    digest: String,
    at: String,
    response: Value,
}

fn with_receipts<T>(store: &Store, f: impl FnOnce(&mut Receipts) -> Result<T>) -> Result<T> {
    crate::management::client_tokens::ensure_private_dir(&store.dir)?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(store.dir.join("tell-receipts.lock"))?;
    lock.lock_exclusive()?;
    let path = store.dir.join("tell-receipts.json");
    let mut receipts: Receipts = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|_| Error::new("persistence_unavailable", 503))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Receipts::default(),
        Err(_) => return Err(Error::new("persistence_unavailable", 503)),
    };
    let result = f(&mut receipts)?;
    crate::config::atomic::write_private_pretty_json_atomic(
        &path,
        &receipts,
        "peer tell receipts",
    )?;
    Ok(result)
}

/// Handle `POST /peer/v1/tells` from an authenticated peer.
pub fn receive(
    store: &Store,
    peer: &Peer,
    data: Value,
    deliver: &dyn Fn(&Delivery<'_>) -> Result<String>,
) -> Result<Value> {
    let request: WireTellV1 =
        serde_json::from_value(data).map_err(|_| Error::new("peer_protocol_error", 400))?;
    if request.schema_version != 1 {
        return Err(Error::new("peer_protocol_error", 400));
    }
    if !valid_tell_id(&request.tell_id)
        || request.text.trim().is_empty()
        || request.text.len() > TEXT_MAX
        || !single_line_id(&request.requester.cutex_session_id)
        || !single_line_id(&request.target)
        || request
            .requester
            .name
            .as_ref()
            .is_some_and(|n| n.len() > 128)
    {
        return Err(Error::new("invalid_request", 400));
    }
    if !store.snapshot()?.settings.enabled {
        return Err(Error::new("peer_unavailable", 409));
    }
    // Same boundary as incoming asks: exported/visible target, accept switch.
    super::asks::validate_inbound(peer, &request.target)?;
    let digest = request.digest();
    let now = chrono::Utc::now();
    with_receipts(store, |receipts| {
        let mine = receipts.peers.entry(peer.peer_id.clone()).or_default();
        mine.retain(|_, r| {
            crate::asks::parse_time(&r.at)
                .is_some_and(|at| now - at < chrono::Duration::days(RECEIPT_DAYS))
        });
        if let Some(existing) = mine.get(&request.tell_id) {
            if existing.digest != digest {
                return Err(Error::new("idempotency_conflict", 409));
            }
            let mut response = existing.response.clone();
            response["deduplicated"] = json!(true);
            return Ok(response);
        }
        let recent = mine
            .values()
            .filter(|r| {
                crate::asks::parse_time(&r.at)
                    .is_some_and(|at| now - at < chrono::Duration::hours(1))
            })
            .count();
        if recent >= peer.policy.inbound.asks_per_hour {
            let mut e = Error::new("peer_rate_limited", 429);
            e.details = json!({"retryAfterSecs":60});
            return Err(e);
        }
        let directory = crate::asks::directory::Directory::load()
            .map_err(|_| Error::new("persistence_unavailable", 503))?;
        let target = directory
            .get(&request.target)
            .ok_or_else(|| Error::new("recipient_outside_boundary", 403))?;
        let facts = crate::agent_bus::peer_tell::PeerTellFacts {
            peer_id: peer.peer_id.clone(),
            host: peer.label.clone(),
            sender_id: request.requester.cutex_session_id.clone(),
            sender_name: request
                .requester
                .name
                .as_deref()
                .map(super::wire::sanitize_name)
                .filter(|n| !n.is_empty()),
            tell_id: request.tell_id.clone(),
        };
        let message_id = format!(
            "{}{}",
            crate::agent_bus::peer_tell::MESSAGE_ID_PREFIX,
            crate::asks::sha256_hex(
                format!("{}\u{1f}{}", peer.peer_id, request.tell_id).as_bytes()
            )
            .get(..32)
            .unwrap_or_default()
        );
        let presence = crate::asks::store::AskStore::open_default()
            .ok()
            .and_then(|s| s.snapshot().ok())
            .and_then(|s| s.presence.get(&request.target).cloned())
            .unwrap_or_default();
        let recipient_state = match (presence.online, presence.busy) {
            (false, _) => "offline",
            (true, true) => "busy",
            (true, false) => "idle",
        };
        let bus_id = deliver(&Delivery {
            target: &request.target,
            text: &request.text,
            facts: &facts,
            message_id: &message_id,
        })?;
        let response = json!({
            "schemaVersion": 1,
            "tellId": request.tell_id,
            "messageId": bus_id,
            "deliveryKind": "note",
            "recipientState": recipient_state,
            "target": {"cutexSessionId": target.cutex_session_id, "name": target.name.as_deref().map(super::wire::sanitize_name)},
        });
        mine.insert(
            request.tell_id.clone(),
            Receipt {
                digest: digest.clone(),
                at: super::now(),
                response: response.clone(),
            },
        );
        if mine.len() > RECEIPTS_PER_PEER {
            let mut by_age: Vec<_> = mine
                .iter()
                .map(|(k, r)| (r.at.clone(), k.clone()))
                .collect();
            by_age.sort();
            for (_, key) in by_age.into_iter().take(mine.len() - RECEIPTS_PER_PEER) {
                mine.remove(&key);
            }
        }
        Ok(response)
    })
}

fn unsupported(detail: &str) -> AskError {
    AskError::new("remote_unsupported", detail)
}

/// Arguments of a cross-host `agent_tell`: `to` and inline `text` only
/// (`wake: false` is accepted). Checked before any ID resolution. A
/// model-supplied `action_id` (the retry key, removed by `tools::handle`
/// but still present when the facade's ID preflight sees the call) is
/// accepted too.
pub fn check_options(args: &Value) -> std::result::Result<(), AskError> {
    let object = args
        .as_object()
        .ok_or_else(|| crate::asks::tools::invalid("arguments must be an object"))?;
    for key in object.keys() {
        match key.as_str() {
            "to" | "text" | "action_id" => {}
            "wake" if object[key] == json!(false) => {}
            "wake" => return Err(unsupported("cross-host tells are notes: wake is unavailable for agents on other hosts (use agent_delegate for work there)")),
            "delegation" | "supersedes" => return Err(unsupported("delegation revisions are unavailable across hosts (remote brief revisions are unsupported)")),
            "attachments" => return Err(unsupported("cross-host tells carry inline text only (no attachments)")),
            other => return Err(crate::asks::tools::invalid(format!("unknown field `{other}`"))),
        }
    }
    Ok(())
}

/// `agent_tell` with `to: name@host`. Only inline text: options that cannot
/// cross hosts are refused before anything is sent or written.
pub fn send(
    ctx: &ToolContext<'_>,
    args: &Value,
    action_id: &str,
) -> std::result::Result<Value, AskError> {
    check_options(args)?;
    let object = args.as_object().expect("checked");
    let to = object["to"].as_str().unwrap_or_default().trim().to_string();
    let text = object
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| crate::asks::tools::invalid("text: 1 byte to 16 KiB"))?
        .to_string();
    if text.trim().is_empty() || text.len() > TEXT_MAX {
        return Err(crate::asks::tools::invalid("text: 1 byte to 16 KiB"));
    }
    let request = json!({"to": to, "text": text, "remote": true});
    crate::asks::tools::once(
        ctx,
        crate::asks::tools::AGENT_TELL,
        action_id,
        &request,
        || {
            let result = (|| -> Result<Value> {
                let (peer, target) = super::asks::resolve(&to)?;
                let target_id = target["cutexSessionId"]
                    .as_str()
                    .ok_or_else(|| Error::new("peer_protocol_error", 502))?
                    .to_string();
                let directory = ctx
                    .world
                    .directory()
                    .map_err(|_| Error::new("persistence_unavailable", 503))?;
                // Deterministic: a retry after a lost response reuses the ID and
                // the receiver answers from its receipt instead of a second note.
                let tell_id = format!(
                    "ptl-{}",
                    crate::asks::sha256_hex(
                        format!(
                            "{}\u{1f}{}\u{1f}{action_id}",
                            super::instance_id(),
                            ctx.caller
                        )
                        .as_bytes()
                    )
                    .get(..32)
                    .unwrap_or_default()
                );
                let wire = WireTellV1 {
                    schema_version: 1,
                    tell_id: tell_id.clone(),
                    requester: WireTellRequesterV1 {
                        cutex_session_id: ctx.caller.into(),
                        name: directory
                            .get(ctx.caller)
                            .and_then(|a| a.name.as_deref())
                            .map(super::wire::sanitize_name),
                    },
                    target: target_id.clone(),
                    text: text.clone(),
                };
                let store = Store::open()?;
                let response = super::client::call(
                    &store,
                    &peer,
                    "POST",
                    ROUTE,
                    Some(&serde_json::to_value(&wire)?),
                )
                .map_err(|e| {
                    if e.status == 404 && e.code == "not_found" {
                        Error::new("remote_unsupported", 404)
                    } else {
                        e
                    }
                })?;
                let name = super::wire::sanitize_name(
                    response["target"]["name"]
                        .as_str()
                        .or_else(|| target["name"].as_str())
                        .unwrap_or(&target_id),
                );
                let state = match response["recipientState"].as_str() {
                    Some(s @ ("idle" | "busy" | "offline")) => s,
                    _ => "offline",
                };
                let address = format!("{name}@{}", peer.label);
                Ok(json!({
                    "ok": true,
                    "to": address,
                    "name": name,
                    "cutexSessionId": target_id,
                    "host": peer.label,
                    "peerId": peer.peer_id,
                    "remote": true,
                    "tellId": tell_id,
                    "deliveryKind": "note",
                    "recipientState": state,
                    "detail": format!("Delivered to host {}. {}", peer.label, crate::asks::tools::tell_detail_for(&address, state, false, false, ctx.facade, false)),
                }))
            })();
            result.map_err(|error| {
            if error.code == "remote_unsupported" && error.status == 404 {
                return unsupported(UNSUPPORTED_DETAIL);
            }
            let transport = error.is_transport();
            let mut e = super::asks::ask_error(error);
            if transport {
                // The note may have arrived; the same action_id retries safely.
                e.detail = "the other host did not answer; it may or may not have the note. Retrying with the same action_id is safe (it is never delivered twice)".into();
                if !e.extra.is_object() {
                    e.extra = json!({});
                }
                e.extra["status"] = json!("response_uncertain");
            }
            e
        })
        },
    )
}
