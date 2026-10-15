//! Agent security settings (`config.json` `agent_security`).
//!
//! Owner decisions (2026-10-07), see `docs/agent-tool-security.md`:
//!
//! - Every agent authenticates to the Agent Bus with its own per-agent
//!   credential. That is not configurable (only a debug-build escape hatch,
//!   [`DEBUG_SHARED_BUS_TOKEN_ENV`], exists for local testing).
//! - The cooperation boundary is the **project**: agents of one project (or,
//!   without a project, of one creator tree) can see and message each other.
//!   Across that boundary they are isolated unless the owner links the two
//!   domains here, or relaxes the boundary to host-wide visibility.
//! - Without linking whole domains, the owner can grant one project
//!   visibility of specific external agents (`external_agents`): the
//!   project's agents list and message the agent and see its own services;
//!   the agent may only message (answer) the project's agents.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Debug builds only: launch agents with the shared root Agent Bus token
/// instead of a per-agent credential (pre-2026-10-07 behaviour). Release
/// builds ignore it.
pub const DEBUG_SHARED_BUS_TOKEN_ENV: &str = "CUTEX_DEBUG_SHARED_AGENT_BUS_TOKEN";

/// Maximum number of explicit domain links.
pub const MAX_DOMAIN_LINKS: usize = 256;

/// Maximum number of per-project external agent grants.
pub const MAX_EXTERNAL_AGENTS: usize = 256;

/// How far agent visibility reaches beyond the caller's own domain.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrossProjectAccess {
    /// Agents see, message and share services only inside their own domain
    /// (project, or creator tree without a project) and explicitly linked
    /// domains.
    #[default]
    Isolated,
    /// Host-wide visibility as before 2026-10-07: Agent Bus groups decide.
    Host,
}

impl CrossProjectAccess {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Isolated => "isolated",
            Self::Host => "host",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "isolated" => Some(Self::Isolated),
            "host" => Some(Self::Host),
            _ => None,
        }
    }
}

/// One cooperation domain: a project, or the creator tree rooted at an agent
/// without a project.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DomainKey {
    Project(String),
    /// Root durable session (`cutex.<uuid>`) of a creator tree.
    Agent(String),
}

impl DomainKey {
    pub fn parse(value: &str) -> Result<Self, String> {
        if let Some(project) = value.strip_prefix("project:") {
            crate::agent_management::ProjectId::new(project.to_string())
                .map_err(|_| format!("invalid project id in domain `{value}`"))?;
            return Ok(Self::Project(project.to_string()));
        }
        if let Some(session) = value.strip_prefix("agent:") {
            if !crate::agent_bus::routing::is_full_durable_cutex_session_id(session) {
                return Err(format!("invalid agent id in domain `{value}` (cutex.<uuid>)"));
            }
            return Ok(Self::Agent(session.to_string()));
        }
        Err(format!(
            "domain `{value}` must be `project:<project id>` or `agent:<cutex session id>`"
        ))
    }
}

impl fmt::Display for DomainKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Project(id) => write!(formatter, "project:{id}"),
            Self::Agent(id) => write!(formatter, "agent:{id}"),
        }
    }
}

impl Serialize for DomainKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for DomainKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

/// An undirected link between two domains (`["project:a", "project:b"]`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainLink(pub DomainKey, pub DomainKey);

impl DomainLink {
    /// Canonical (sorted) pair so links compare independent of order.
    pub fn canonical(&self) -> (DomainKey, DomainKey) {
        if self.0 <= self.1 {
            (self.0.clone(), self.1.clone())
        } else {
            (self.1.clone(), self.0.clone())
        }
    }
}

/// A directional grant: agents of `project` may see (list, message, see the
/// own services of) the external agent `agent`; `agent` may only message the
/// project's agents. Serialized as `{"project": "<id>", "agent": "cutex.<uuid>"}`
/// (the same in `config.json` and the Management API).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct ExternalAgentGrant {
    pub project: String,
    pub agent: String,
}

impl<'de> Deserialize<'de> for ExternalAgentGrant {
    /// Objects only (no serde sequence form), no unknown fields, known id
    /// formats: `project` a valid project id, `agent` `cutex.<uuid>`.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            project: String,
            agent: String,
        }
        use serde::de::Error;
        let object = serde_json::Map::<String, serde_json::Value>::deserialize(deserializer)?;
        let raw: Raw =
            serde_json::from_value(serde_json::Value::Object(object)).map_err(D::Error::custom)?;
        crate::agent_management::ProjectId::new(raw.project.clone())
            .map_err(|_| D::Error::custom(format!("invalid project id `{}`", raw.project)))?;
        if !crate::agent_bus::routing::is_full_durable_cutex_session_id(&raw.agent) {
            return Err(D::Error::custom(format!(
                "invalid agent id `{}` (cutex.<uuid>)",
                raw.agent
            )));
        }
        Ok(Self {
            project: raw.project,
            agent: raw.agent,
        })
    }
}

/// `config.json` `agent_security`. Unknown fields are tolerated (no
/// `deny_unknown_fields`) so a rollback to an older cutex never fails to
/// parse the whole config.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSecurityConfig {
    #[serde(default, skip_serializing_if = "is_isolated")]
    pub cross_project: CrossProjectAccess,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<DomainLink>,
    /// Per-project grants of specific external agents (directional).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub external_agents: Vec<ExternalAgentGrant>,
}

fn is_isolated(value: &CrossProjectAccess) -> bool {
    *value == CrossProjectAccess::Isolated
}

impl AgentSecurityConfig {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.links.len() <= MAX_DOMAIN_LINKS,
            "agentSecurity.links allows at most {MAX_DOMAIN_LINKS} entries"
        );
        let mut seen = BTreeSet::new();
        for link in &self.links {
            anyhow::ensure!(link.0 != link.1, "agentSecurity.links: a domain cannot link to itself");
            anyhow::ensure!(
                seen.insert(link.canonical()),
                "agentSecurity.links: duplicate link {} <-> {}",
                link.0,
                link.1
            );
        }
        anyhow::ensure!(
            self.external_agents.len() <= MAX_EXTERNAL_AGENTS,
            "agentSecurity.externalAgents allows at most {MAX_EXTERNAL_AGENTS} entries"
        );
        let mut grants = BTreeSet::new();
        for grant in &self.external_agents {
            anyhow::ensure!(
                grants.insert(grant),
                "agentSecurity.externalAgents: duplicate grant of {} to project {}",
                grant.agent,
                grant.project
            );
        }
        Ok(())
    }

    /// `(project, agent)` grant set for lookups.
    pub fn external_agent_set(&self) -> BTreeSet<(String, String)> {
        self.external_agents
            .iter()
            .map(|grant| (grant.project.clone(), grant.agent.clone()))
            .collect()
    }

    /// Canonical link set for lookups.
    pub fn link_set(&self) -> BTreeSet<(DomainKey, DomainKey)> {
        self.links.iter().map(DomainLink::canonical).collect()
    }
}

/// Debug-build escape hatch: agents get the shared root token.
pub fn debug_shared_bus_token() -> bool {
    cfg!(debug_assertions)
        && std::env::var(DEBUG_SHARED_BUS_TOKEN_ENV).is_ok_and(|value| value == "1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_isolated_and_serialize_empty() {
        let config = AgentSecurityConfig::default();
        assert_eq!(config.cross_project, CrossProjectAccess::Isolated);
        assert_eq!(serde_json::to_value(&config).unwrap(), serde_json::json!({}));
        let parsed: AgentSecurityConfig = serde_json::from_value(serde_json::json!({
            "cross_project": "host",
            "links": [["project:alpha", "agent:cutex.00000000-0000-0000-0000-000000000001"]],
            "future_field": true,
        }))
        .unwrap();
        assert_eq!(parsed.cross_project, CrossProjectAccess::Host);
        assert_eq!(parsed.links.len(), 1);
        assert_eq!(
            serde_json::to_value(&parsed).unwrap()["links"][0][1],
            "agent:cutex.00000000-0000-0000-0000-000000000001"
        );
    }

    #[test]
    fn links_are_validated() {
        for bad in [
            serde_json::json!({"links": [["alpha", "project:beta"]]}),
            serde_json::json!({"links": [["agent:not-a-session", "project:beta"]]}),
            serde_json::json!({"links": [["project:bad id", "project:beta"]]}),
        ] {
            assert!(serde_json::from_value::<AgentSecurityConfig>(bad).is_err());
        }
        let duplicate: AgentSecurityConfig = serde_json::from_value(serde_json::json!({
            "links": [["project:a", "project:b"], ["project:b", "project:a"]]
        }))
        .unwrap();
        assert!(duplicate.validate().is_err());
        let self_link: AgentSecurityConfig =
            serde_json::from_value(serde_json::json!({"links": [["project:a", "project:a"]]}))
                .unwrap();
        assert!(self_link.validate().is_err());
    }

    #[test]
    fn external_agent_grants_parse_validate_and_round_trip() {
        const AGENT: &str = "cutex.00000000-0000-0000-0000-000000000001";
        let parsed: AgentSecurityConfig = serde_json::from_value(serde_json::json!({
            "external_agents": [{"project": "alpha", "agent": AGENT}],
        }))
        .unwrap();
        parsed.validate().unwrap();
        assert_eq!(
            parsed.external_agent_set(),
            BTreeSet::from([("alpha".to_string(), AGENT.to_string())])
        );
        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            serde_json::json!({"external_agents": [{"project": "alpha", "agent": AGENT}]})
        );
        for bad in [
            serde_json::json!({"external_agents": [{"project": "bad id", "agent": AGENT}]}),
            serde_json::json!({"external_agents": [{"project": "alpha", "agent": "agent:x"}]}),
            serde_json::json!({"external_agents": [{"project": "alpha", "agent": "cutex.abc"}]}),
            serde_json::json!({"external_agents": [{"project": "alpha"}]}),
            serde_json::json!({"external_agents": [{"project": "alpha", "agent": AGENT, "mode": "x"}]}),
            serde_json::json!({"external_agents": [["alpha", AGENT]]}),
        ] {
            assert!(serde_json::from_value::<AgentSecurityConfig>(bad.clone()).is_err(), "{bad}");
        }
        let duplicate: AgentSecurityConfig = serde_json::from_value(serde_json::json!({
            "external_agents": [
                {"project": "alpha", "agent": AGENT},
                {"project": "alpha", "agent": AGENT},
            ],
        }))
        .unwrap();
        assert!(duplicate.validate().is_err());
        let too_many = AgentSecurityConfig {
            external_agents: (0..=MAX_EXTERNAL_AGENTS)
                .map(|index| ExternalAgentGrant {
                    project: format!("p{index}"),
                    agent: AGENT.to_string(),
                })
                .collect(),
            ..AgentSecurityConfig::default()
        };
        assert!(too_many.validate().is_err());
    }

    #[test]
    fn escape_hatch_is_off_without_the_variable() {
        // The variable is never set by the test harness.
        if std::env::var(DEBUG_SHARED_BUS_TOKEN_ENV).is_err() {
            assert!(!debug_shared_bus_token());
        }
    }
}
