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
pub fn can_deliver(peer: &super::store::Peer, agent: &crate::asks::directory::AgentInfo) -> bool {
    can_deliver_with(peer, agent, |user| {
        crate::management::users::UserStore::open_default()
            .and_then(|users| users.snapshot())
            .ok()
            .and_then(|state| state.user(user).and_then(|u| u.grant.clone()))
            .map(|grant| crate::management::users::Scope::cached(&grant))
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
        || scope_of(&peer.user_id).is_some_and(|scope| scope.grant.send)
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
