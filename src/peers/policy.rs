use super::{Error, Result};
use serde::{Deserialize, Serialize};

/// One authorization boundary for peer directory visibility. The candidate must
/// come from the local Directory, never a cached remote directory or mirror ask.
/// A peer bound to `owner` uses its export policy; a peer bound to another
/// local user sees exactly that user's grant (local sessions only, so a
/// third host's agents are never re-exported).
pub fn visible(peer: &super::store::Peer, agent: &crate::asks::directory::AgentInfo) -> bool {
    visible_with(peer, agent, |user| {
        crate::management::users::UserStore::open_default()
            .and_then(|users| users.snapshot())
            .ok()
            .and_then(|state| state.user(user).and_then(|u| u.grant.clone()))
            .map(|grant| crate::management::users::Scope::cached(&grant))
    })
}

/// Sending is a separate permission over the same local visibility boundary:
/// the peer-level switch and, for a non-owner binding, the user's `send`.
/// r290 P1: decided on the current membership (a fresh scope, not the
/// directory cache), so an agent just moved into a viewer project is not
/// sent to under its old project's role.
pub fn can_deliver(peer: &super::store::Peer, agent: &crate::asks::directory::AgentInfo) -> bool {
    let fresh = std::cell::OnceCell::new();
    can_deliver_with(peer, agent, |user| {
        fresh.get_or_init(|| {
            crate::management::users::UserStore::open_default()
                .and_then(|users| users.snapshot())
                .ok()
                .and_then(|state| state.user(user).and_then(|u| u.grant.clone()))
                .and_then(|grant| crate::management::users::Scope::load(&grant).ok())
        })
        .clone()
    })
}

pub(crate) fn visible_with(
    peer: &super::store::Peer,
    agent: &crate::asks::directory::AgentInfo,
    scope_of: impl FnOnce(&str) -> Option<crate::management::users::Scope>,
) -> bool {
    if !(peer.enabled && peer.pending_until.is_none() && peer.status != "removedByPeer") {
        return false;
    }
    if peer.user_id == super::store::OWNER_USER_ID {
        return peer.policy.exports(agent);
    }
    if !crate::management::users::valid_user_id(&peer.user_id) {
        return false;
    }
    !agent.archived
        && agent.agent_enabled
        && scope_of(&peer.user_id).is_some_and(|scope| scope.sees_session(&agent.cutex_session_id))
}

pub(crate) fn can_deliver_with(
    peer: &super::store::Peer,
    agent: &crate::asks::directory::AgentInfo,
    scope_of: impl Fn(&str) -> Option<crate::management::users::Scope>,
) -> bool {
    if !peer.policy.inbound.asks || !visible_with(peer, agent, &scope_of) {
        return false;
    }
    peer.user_id == super::store::OWNER_USER_ID
        || scope_of(&peer.user_id).is_some_and(|scope| scope.may_send(&agent.cutex_session_id))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub enabled: bool,
    pub outbox_max_age_hours: u32,
    pub max_hops: u32,
    pub refresh_agents_secs: u32,
    #[serde(default = "connect_timeout")]
    pub connect_timeout_secs: u32,
    #[serde(default = "read_timeout")]
    pub read_timeout_secs: u32,
}
fn connect_timeout() -> u32 {
    5
}
fn read_timeout() -> u32 {
    30
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            outbox_max_age_hours: 24,
            max_hops: 3,
            refresh_agents_secs: 60,
            connect_timeout_secs: 5,
            read_timeout_secs: 30,
        }
    }
}
impl Settings {
    pub fn patched(&self, value: &serde_json::Value) -> Result<Self> {
        let mut base = serde_json::to_value(self)?;
        for (key, value) in value
            .as_object()
            .ok_or_else(|| Error::new("invalid_request", 400))?
        {
            // Persisted config may contain fields from a newer daemon; a PATCH
            // must still reject typos instead of silently dropping them.
            if base.get(key).is_none() {
                return Err(Error::new("invalid_request", 400));
            }
            base[key] = value.clone();
        }
        let next: Self = serde_json::from_value(base)?;
        if !(1..=720).contains(&next.outbox_max_age_hours)
            || !(1..=8).contains(&next.max_hops)
            || !(10..=3600).contains(&next.refresh_agents_secs)
            || !(1..=60).contains(&next.connect_timeout_secs)
            || !(1..=300).contains(&next.read_timeout_secs)
        {
            return Err(Error::new("invalid_request", 400));
        }
        Ok(next)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Review {
    Always,
    IfTargetWider,
    Never,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Visibility {
    None,
    All,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Export {
    Agent { agent: String },
    Project { project: String },
    All { all: bool },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Inbound {
    pub exports: Vec<Export>,
    #[serde(rename = "accept", alias = "asks")]
    pub asks: bool,
    pub assumed_caps: Caps,
    pub review: Review,
    pub max_open_asks: usize,
    pub asks_per_hour: usize,
    /// Cross-host `agent_tell` with `wake:true` may start a turn of an agent
    /// this peer can reach (default off; older configs read as off).
    pub tell_wake: bool,
    /// Wakes from this peer per hour (counted from tell receipts); beyond
    /// it a wake is delivered as a note (`wakeDowngraded`, `rate_limited`).
    pub tell_wakes_per_hour: usize,
}
impl Default for Inbound {
    fn default() -> Self {
        Self {
            exports: vec![],
            asks: true,
            review: Review::Always,
            assumed_caps: Caps {
                sandbox: Some("read-only".into()),
                network_access: false,
                unrestricted_services: false,
            },
            max_open_asks: 20,
            asks_per_hour: 60,
            tell_wake: false,
            tell_wakes_per_hour: 60,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Outbound {
    pub visibility: Visibility,
}
impl Default for Outbound {
    fn default() -> Self {
        Self {
            visibility: Visibility::None,
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Policy {
    pub inbound: Inbound,
    pub outbound: Outbound,
}

// Persistence tolerates additive fields from newer daemons. Mutations use an
// explicit allowlist at every object level so typos never disappear silently.
fn patch_fields<'a>(
    value: &'a serde_json::Value,
    allowed: &[&str],
) -> Result<&'a serde_json::Map<String, serde_json::Value>> {
    let fields = value
        .as_object()
        .ok_or_else(|| Error::new("invalid_request", 400))?;
    if fields.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(Error::new("invalid_request", 400));
    }
    Ok(fields)
}

impl Policy {
    pub fn needs_review(&self, target: &Caps) -> bool {
        needs_review(&self.inbound.review, &self.inbound.assumed_caps, target)
    }
    fn exports(&self, agent: &crate::asks::directory::AgentInfo) -> bool {
        !agent.archived
            && agent.agent_enabled
            && self.inbound.exports.iter().any(|item| match item {
                Export::All { all } => *all,
                Export::Agent { agent: id } => id == &agent.cutex_session_id,
                Export::Project { project } => agent.project.as_ref() == Some(project),
            })
    }
    pub fn patched(&self, patch: &serde_json::Value) -> Result<Self> {
        let mut value = serde_json::to_value(self)?;
        for (key, fields) in patch_fields(patch, &["inbound", "outbound"])? {
            let allowed = if key == "inbound" {
                &[
                    "exports",
                    "accept",
                    "asks",
                    "assumedCaps",
                    "review",
                    "maxOpenAsks",
                    "asksPerHour",
                    "tellWake",
                    "tellWakesPerHour",
                ][..]
            } else {
                &["visibility"][..]
            };
            let fields = patch_fields(fields, allowed)?;
            if key == "inbound" && fields.contains_key("accept") && fields.contains_key("asks") {
                return Err(Error::new("invalid_request", 400));
            }
            for (field, v) in fields {
                if key == "inbound" && field == "exports" {
                    let exports = v
                        .as_array()
                        .ok_or_else(|| Error::new("invalid_request", 400))?;
                    for export in exports {
                        if patch_fields(export, &["agent", "project", "all"])?.len() != 1 {
                            return Err(Error::new("invalid_request", 400));
                        }
                    }
                }
                if key == "inbound" && field == "assumedCaps" {
                    patch_fields(v, &["sandbox", "networkAccess", "unrestrictedServices"])?;
                }
                let field = if key == "inbound" && field == "asks" {
                    "accept"
                } else {
                    field.as_str()
                };
                value[key][field] = v.clone();
            }
        }
        let next: Self = serde_json::from_value(value)?;
        if next.inbound.max_open_asks == 0
            || next.inbound.max_open_asks > 10000
            || next.inbound.asks_per_hour == 0
            || next.inbound.asks_per_hour > 100000
            || next.inbound.tell_wakes_per_hour == 0
            || next.inbound.tell_wakes_per_hour > 100000
        {
            return Err(Error::new("invalid_request", 400));
        }
        Ok(next)
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Caps {
    pub sandbox: Option<String>,
    pub network_access: bool,
    pub unrestricted_services: bool,
}
impl From<&crate::asks::caps::Capabilities> for Caps {
    fn from(c: &crate::asks::caps::Capabilities) -> Self {
        Self {
            sandbox: c.sandbox.map(str::to_string),
            network_access: c.network_access,
            unrestricted_services: c.unrestricted_services,
        }
    }
}
pub fn needs_review(review: &Review, requester: &Caps, target: &Caps) -> bool {
    match review {
        Review::Always => true,
        Review::Never => false,
        Review::IfTargetWider => {
            let level = |s: &Option<String>| {
                s.as_deref().and_then(|s| match s {
                    "read-only" => Some(0),
                    "workspace-write" => Some(1),
                    "danger-full-access" | "full-access" => Some(2),
                    _ => None,
                })
            };
            match (level(&requester.sandbox), level(&target.sandbox)) {
                (Some(a), Some(b)) => {
                    b > a
                        || target.network_access && !requester.network_access
                        || target.unrestricted_services && !requester.unrestricted_services
                }
                _ => true,
            }
        }
    }
}

// r290 P10 (c) ------------------------------------------ own-host preset
//
// "这是我自己的主机": a one-click owner-to-owner policy for a peer bound to
// `owner`. Pairing starts fully closed (no exports, every ask reviewed, peer
// agents invisible); between two hosts of the same owner that is friction,
// not protection. The preset opens:
// - inbound: every active local agent is visible to the peer (exports all),
//   asks and tells are accepted, wakes are allowed (tellWake), and review is
//   only needed when the target agent is wider than `workspace-write`
//   without network (full-access, network or unrestricted services still
//   go to the owner);
// - outbound: this host's agents see the peer's agents.
// Rate limits stay as they are. URLs and transport are untouched: peer
// tokens still never travel over plain http on the LAN (client::addresses).
// It applies only on this host (each host sets its own inbound policy) and
// only to an owner-bound peer. `verified` says whether the peer's owner keys
// (exchanged at pairing) include one of this host's owner keys; when not,
// the preset is the owner's explicit choice on this host.
impl Policy {
    pub fn own_host(&self) -> Self {
        let mut next = self.clone();
        next.inbound.exports = vec![Export::All { all: true }];
        next.inbound.asks = true;
        next.inbound.review = Review::IfTargetWider;
        next.inbound.assumed_caps = Caps {
            sandbox: Some("workspace-write".into()),
            network_access: false,
            unrestricted_services: false,
        };
        next.inbound.tell_wake = true;
        next.outbound.visibility = Visibility::All;
        next
    }
}

/// Does any of the peer's owner keys belong to this host's owner?
pub(crate) fn owner_keys_overlap(local: &[String], remote: &[String]) -> bool {
    !local.is_empty() && remote.iter().any(|key| local.contains(key))
}

/// The peer's owner keys recorded at pairing (`ownerPersonKeys`) include one
/// of this host's owner keys.
pub fn own_host_verified(peer_id: &str) -> bool {
    let remote = crate::management::users::UserStore::open_default()
        .and_then(|users| users.persons_snapshot())
        .ok()
        .and_then(|persons| persons.peer_persons.get(peer_id).cloned())
        .unwrap_or_default();
    owner_keys_overlap(&crate::management::persons::local_owner_keys(), &remote)
}

/// Owner-local, full-scope only: the preset widens what the peer reaches.
pub fn own_host_allowed() -> bool {
    crate::management::users::request_is_owner_local()
        && crate::management::listeners::current_client_token().is_none_or(|token| token.scope.is_full())
}

/// Apply the preset to an owner-bound peer. Returns the updated peer.
pub fn apply_own_host(store: &super::store::Store, id: &str) -> Result<super::store::Peer> {
    store.with(|s| {
        let peer = s.peers.get_mut(id).ok_or_else(|| Error::new("peer_not_found", 404))?;
        if peer.user_id != super::store::OWNER_USER_ID {
            let mut error = Error::new("peer_not_owner_bound", 409);
            error.details = serde_json::json!({ "userId": peer.user_id });
            return Err(error);
        }
        peer.policy = peer.policy.own_host();
        peer.policy_revision += 1;
        Ok(peer.clone())
    })
}

/// `ownHost` of a pairing body: absent / false / true; anything else is 400.
pub fn own_host_requested(body: &serde_json::Value) -> Result<bool> {
    match body.get("ownHost") {
        None | Some(serde_json::Value::Null) => Ok(false),
        Some(serde_json::Value::Bool(value)) => Ok(*value),
        Some(_) => Err(Error::new("invalid_request", 400)),
    }
}

#[cfg(test)]
mod own_host_tests {
    use super::*;

    #[test]
    fn own_host_opens_exports_and_visibility_but_keeps_review_for_wider_targets() {
        let policy = Policy::default().own_host();
        assert_eq!(policy.inbound.exports, vec![Export::All { all: true }]);
        assert_eq!(policy.outbound.visibility, Visibility::All);
        assert!(policy.inbound.asks && policy.inbound.tell_wake);
        let caps = |sandbox: &str, network: bool| Caps { sandbox: Some(sandbox.into()), network_access: network, unrestricted_services: false };
        assert!(!policy.needs_review(&caps("read-only", false)));
        assert!(!policy.needs_review(&caps("workspace-write", false)));
        assert!(policy.needs_review(&caps("workspace-write", true)), "network still reviewed");
        assert!(policy.needs_review(&caps("danger-full-access", false)), "full access still reviewed");
        assert!(policy.needs_review(&Caps { sandbox: None, network_access: false, unrestricted_services: false }));
        let mut limited = Policy::default();
        limited.inbound.asks_per_hour = 7;
        assert_eq!(limited.own_host().inbound.asks_per_hour, 7, "rate limits are kept");
        // The default stays fully closed.
        let closed = Policy::default();
        assert!(closed.inbound.exports.is_empty() && closed.outbound.visibility == Visibility::None);
        assert!(closed.needs_review(&caps("read-only", false)));
    }

    #[test]
    fn owner_keys_must_overlap_to_verify() {
        let local = vec!["k1".to_string(), "k2".to_string()];
        assert!(owner_keys_overlap(&local, &["k2".to_string()]));
        assert!(!owner_keys_overlap(&local, &["k3".to_string()]));
        assert!(!owner_keys_overlap(&local, &[]));
        assert!(!owner_keys_overlap(&[], &["k1".to_string()]));
    }

    #[test]
    fn own_host_flag_is_a_strict_boolean() {
        assert!(!own_host_requested(&serde_json::json!({})).unwrap());
        assert!(own_host_requested(&serde_json::json!({ "ownHost": true })).unwrap());
        assert!(own_host_requested(&serde_json::json!({ "ownHost": "yes" })).is_err());
    }

    #[test]
    fn own_host_is_owner_local_full_scope_only() {
        use crate::management::client_tokens::{DeviceScope, IssuedVia, VerifiedClientToken};
        use crate::management::listeners::{set_current_client_token, set_current_connection, ConnectionInfo};
        let token = |scope, via| { let mut t = VerifiedClientToken::test_token("ct-own-host"); t.scope = scope; t.issued_via = via; t };
        let connection = |remote| Some(ConnectionInfo { remote, peer: "127.0.0.1:50000".parse().unwrap() });
        set_current_connection(connection(false));
        set_current_client_token(None);
        assert!(own_host_allowed(), "root credential on loopback");
        set_current_client_token(Some(token(DeviceScope::Full, IssuedVia::HostCli)));
        assert!(own_host_allowed(), "host-issued owner token on loopback (SSH desktop)");
        set_current_client_token(Some(token(DeviceScope::Chat, IssuedVia::HostCli)));
        assert!(!own_host_allowed(), "narrowed token");
        set_current_client_token(Some(token(DeviceScope::Full, IssuedVia::Pairing)));
        assert!(!own_host_allowed(), "paired device token");
        set_current_connection(connection(true));
        set_current_client_token(None);
        assert!(!own_host_allowed(), "remote listener");
        set_current_connection(None);
    }
}
