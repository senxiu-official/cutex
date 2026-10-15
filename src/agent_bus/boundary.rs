//! Agent cooperation boundary (owner decision 2026-10-07).
//!
//! Inside one project agents cooperate like a main agent and its subagents:
//! they list each other, message each other and see the project's services
//! (and the project memory, which cutex injects at launch). An agent without a
//! project is bounded by its creator tree: itself, the agents it created and
//! their descendants, as long as they have no project of their own.
//!
//! Across those domains agents are isolated: not listed, not messageable, no
//! shared services. The owner can link two domains
//! (`agent_security.links`), or relax isolation to host-wide visibility
//! (`agent_security.cross_project = "host"`, where Agent Bus groups decide as
//! before). Authentication is never relaxed (`agent_auth`); authority inside a
//! project stays role-based (`tool_policy`, Agent Management provider).
//!
//! Between "isolated" and "linked" the owner can grant a project specific
//! external agents (`agent_security.external_agents`, directional): the
//! project's agents may see the granted agent (list it, message it, see its
//! own services), the granted agent may only message the project's agents so
//! it can answer. Hence two checks: [`AgentBoundary::may_see`] (listing,
//! services, alarm targets by name) and [`AgentBoundary::may_message`] (send
//! and alarm targets by id).

use std::collections::{BTreeSet, HashMap};

use crate::agent_management::AgentManagementSnapshot;
use crate::config::agent_security::{AgentSecurityConfig, CrossProjectAccess, DomainKey};

/// Upper bound when walking creator links (guards corrupt cycles).
const MAX_CREATOR_DEPTH: usize = 64;

#[derive(Clone, Debug, Default)]
pub struct AgentBoundary {
    access: CrossProjectAccess,
    /// Durable session -> its current project.
    project_of: HashMap<String, String>,
    /// Durable session -> the session that created it.
    creator_of: HashMap<String, String>,
    links: BTreeSet<(DomainKey, DomainKey)>,
    /// `(project, external agent)` grants.
    external_agents: BTreeSet<(String, String)>,
}

impl AgentBoundary {
    pub fn new(
        security: &AgentSecurityConfig,
        project_of: HashMap<String, String>,
        creator_of: HashMap<String, String>,
    ) -> Self {
        Self {
            access: security.cross_project,
            project_of,
            creator_of,
            links: security.link_set(),
            external_agents: security.external_agent_set(),
        }
    }

    /// Domains from the Agent Management store: current project memberships,
    /// project Directors, and creation provenance.
    pub fn from_snapshot(security: &AgentSecurityConfig, snapshot: &AgentManagementSnapshot) -> Self {
        let mut project_of = HashMap::new();
        let mut creator_of = HashMap::new();
        for agent in snapshot.agents.values() {
            if agent.retired_at.is_some() {
                continue;
            }
            let session = agent.cutex_session_id.as_str().to_string();
            if let Some(project) = crate::agent_management::current_project_id(snapshot, agent)
                .filter(|project| crate::memory::project_is_known(snapshot, project))
            {
                project_of.insert(session.clone(), project.as_str().to_string());
            }
            if let Some(creator) = agent
                .created_by_operator_session
                .as_ref()
                .or(agent.created_by_director_session.as_ref())
            {
                creator_of.insert(session, creator.as_str().to_string());
            }
        }
        for authority in snapshot.projects.values() {
            if crate::memory::project_is_known(snapshot, &authority.project_id) {
                // A member's own membership wins (same precedence as memory).
                project_of
                    .entry(authority.authorized_director_session.as_str().to_string())
                    .or_insert_with(|| authority.project_id.as_str().to_string());
            }
        }
        Self::new(security, project_of, creator_of)
    }

    /// Current configuration and Agent Management store. In host-wide mode
    /// the store is not needed and not read.
    pub fn load() -> anyhow::Result<Self> {
        let config = crate::config::store::load_codez_config();
        Self::load_with(&config.agent_security)
    }

    pub fn load_with(security: &AgentSecurityConfig) -> anyhow::Result<Self> {
        if security.cross_project == CrossProjectAccess::Host {
            return Ok(Self::new(security, HashMap::new(), HashMap::new()));
        }
        let snapshot = crate::memory::load_agent_management_snapshot()?;
        Ok(Self::from_snapshot(security, &snapshot))
    }

    /// Host-wide visibility: Agent Bus groups decide, as before 2026-10-07.
    pub fn host_wide(&self) -> bool {
        self.access == CrossProjectAccess::Host
    }

    pub fn project_of(&self, cutex_session_id: &str) -> Option<&str> {
        self.project_of.get(cutex_session_id).map(String::as_str)
    }

    /// The cooperation domain of a durable session.
    pub fn domain(&self, cutex_session_id: &str) -> DomainKey {
        if let Some(project) = self.project_of.get(cutex_session_id) {
            return DomainKey::Project(project.clone());
        }
        let mut root = cutex_session_id;
        for _ in 0..MAX_CREATOR_DEPTH {
            match self.creator_of.get(root) {
                // A creator inside a project is another domain: a detached
                // member does not keep its old project's visibility.
                Some(creator) if !self.project_of.contains_key(creator) && creator != root => {
                    root = creator
                }
                _ => break,
            }
        }
        DomainKey::Agent(root.to_string())
    }

    fn linked(&self, left: &DomainKey, right: &DomainKey) -> bool {
        let pair = if left <= right {
            (left.clone(), right.clone())
        } else {
            (right.clone(), left.clone())
        };
        self.links.contains(&pair)
    }

    /// Mutual cooperation: same agent, same domain or linked domains (or
    /// host-wide mode). Symmetric; external agent grants never give it.
    pub fn may_interact(&self, caller: &str, other: &str) -> bool {
        if self.host_wide() || caller == other {
            return true;
        }
        let (left, right) = (self.domain(caller), self.domain(other));
        left == right || self.linked(&left, &right)
    }

    /// Whether the owner granted `caller`'s project the external agent
    /// `other` (only agents currently in that project benefit).
    pub fn external_grant(&self, caller: &str, other: &str) -> bool {
        !self.host_wide()
            && self.project_of.get(caller).is_some_and(|project| {
                self.external_agents
                    .contains(&(project.clone(), other.to_string()))
            })
    }

    /// Whether `caller` may see `other`: list it, and see the services it
    /// owns. Mutual cooperation, or `other` is an external agent granted to
    /// `caller`'s project.
    pub fn may_see(&self, caller: &str, other: &str) -> bool {
        self.may_interact(caller, other) || self.external_grant(caller, other)
    }

    /// Whether `caller` may message `other` (send, alarm target): either side
    /// may see the other. A granted external agent may thus answer the
    /// project's agents without seeing them.
    pub fn may_message(&self, caller: &str, other: &str) -> bool {
        self.may_see(caller, other) || self.external_grant(other, caller)
    }

    /// Every domain `cutex_session_id` may interact with: its own and the
    /// domains linked to it (`None` in host-wide mode, where reach is the
    /// whole host). The ask gate compares reach (asks §4.4).
    pub fn reach(&self, cutex_session_id: &str) -> Option<BTreeSet<DomainKey>> {
        if self.host_wide() {
            return None;
        }
        let own = self.domain(cutex_session_id);
        let mut reach = BTreeSet::from([own.clone()]);
        for (left, right) in &self.links {
            if *left == own {
                reach.insert(right.clone());
            } else if *right == own {
                reach.insert(left.clone());
            }
        }
        Some(reach)
    }

    /// Projects whose services the caller may see: its own and linked ones
    /// (external agent grants add the granted agent's own services only).
    pub fn visible_projects(&self, caller: &str) -> BTreeSet<String> {
        let own = self.domain(caller);
        let mut projects = BTreeSet::new();
        if let DomainKey::Project(project) = &own {
            projects.insert(project.clone());
        }
        for (left, right) in &self.links {
            let other = if *left == own {
                right
            } else if *right == own {
                left
            } else {
                continue;
            };
            if let DomainKey::Project(project) = other {
                projects.insert(project.clone());
            }
        }
        projects
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::agent_security::DomainLink;

    const DIRECTOR_A: &str = "cutex.00000000-0000-0000-0000-00000000000a";
    const MEMBER_A: &str = "cutex.00000000-0000-0000-0000-0000000000a1";
    const DIRECTOR_B: &str = "cutex.00000000-0000-0000-0000-00000000000b";
    const SOLO: &str = "cutex.00000000-0000-0000-0000-000000000005";
    const SOLO_CHILD: &str = "cutex.00000000-0000-0000-0000-000000000051";
    const DETACHED: &str = "cutex.00000000-0000-0000-0000-0000000000d1";
    const OTHER_SOLO: &str = "cutex.00000000-0000-0000-0000-000000000006";

    fn boundary(security: AgentSecurityConfig) -> AgentBoundary {
        let project_of = [(DIRECTOR_A, "alpha"), (MEMBER_A, "alpha"), (DIRECTOR_B, "beta")]
            .into_iter()
            .map(|(s, p)| (s.to_string(), p.to_string()))
            .collect();
        let creator_of = [(MEMBER_A, DIRECTOR_A), (SOLO_CHILD, SOLO), (DETACHED, DIRECTOR_A)]
            .into_iter()
            .map(|(s, c)| (s.to_string(), c.to_string()))
            .collect();
        AgentBoundary::new(&security, project_of, creator_of)
    }

    #[test]
    fn projects_and_creator_trees_are_isolated_by_default() {
        let b = boundary(AgentSecurityConfig::default());
        assert!(b.may_interact(DIRECTOR_A, MEMBER_A));
        assert!(b.may_interact(MEMBER_A, DIRECTOR_A));
        assert!(!b.may_interact(MEMBER_A, DIRECTOR_B));
        assert!(!b.may_interact(DIRECTOR_A, SOLO));
        // Creator tree for agents without a project.
        assert!(b.may_interact(SOLO, SOLO_CHILD));
        assert!(!b.may_interact(SOLO, OTHER_SOLO));
        // A detached member is its own domain.
        assert_eq!(b.domain(DETACHED), DomainKey::Agent(DETACHED.into()));
        assert!(!b.may_interact(DETACHED, DIRECTOR_A));
        assert!(b.may_interact(SOLO, SOLO));
        assert_eq!(b.visible_projects(MEMBER_A), BTreeSet::from(["alpha".to_string()]));
        assert!(b.visible_projects(SOLO).is_empty());
    }

    #[test]
    fn links_and_host_mode_relax_isolation() {
        let linked = boundary(AgentSecurityConfig {
            links: vec![
                DomainLink(DomainKey::Project("beta".into()), DomainKey::Project("alpha".into())),
                DomainLink(DomainKey::Project("alpha".into()), DomainKey::Agent(SOLO.into())),
            ],
            ..AgentSecurityConfig::default()
        });
        assert!(linked.may_interact(MEMBER_A, DIRECTOR_B));
        assert!(linked.may_interact(DIRECTOR_B, MEMBER_A));
        assert!(linked.may_interact(SOLO_CHILD, MEMBER_A));
        // Links are not transitive.
        assert!(!linked.may_interact(DIRECTOR_B, SOLO));
        assert_eq!(
            linked.visible_projects(MEMBER_A),
            BTreeSet::from(["alpha".to_string(), "beta".to_string()])
        );
        assert_eq!(linked.visible_projects(SOLO), BTreeSet::from(["alpha".to_string()]));
        let host = boundary(AgentSecurityConfig {
            cross_project: CrossProjectAccess::Host,
            ..AgentSecurityConfig::default()
        });
        assert!(host.host_wide());
        assert!(host.may_interact(MEMBER_A, OTHER_SOLO));
    }

    #[test]
    fn external_agent_grants_are_directional_and_revocable() {
        use crate::config::agent_security::ExternalAgentGrant;
        let granted = boundary(AgentSecurityConfig {
            external_agents: vec![ExternalAgentGrant {
                project: "alpha".into(),
                agent: SOLO.into(),
            }],
            ..AgentSecurityConfig::default()
        });
        // Project alpha's agents see and message the granted agent.
        for member in [DIRECTOR_A, MEMBER_A] {
            assert!(granted.external_grant(member, SOLO));
            assert!(granted.may_see(member, SOLO));
            assert!(granted.may_message(member, SOLO));
            // The granted agent answers but does not see the project.
            assert!(!granted.may_see(SOLO, member));
            assert!(granted.may_message(SOLO, member));
            assert!(!granted.may_interact(member, SOLO));
            assert!(!granted.may_interact(SOLO, member));
        }
        // The grant is the agent, not its creator tree or domain.
        assert!(!granted.may_see(MEMBER_A, SOLO_CHILD));
        assert!(!granted.may_message(SOLO_CHILD, MEMBER_A));
        // Other projects and detached former members get nothing.
        assert!(!granted.may_see(DIRECTOR_B, SOLO));
        assert!(!granted.may_message(SOLO, DIRECTOR_B));
        assert!(!granted.may_see(DETACHED, SOLO));
        // No project services through a grant.
        assert!(granted.visible_projects(SOLO).is_empty());
        assert_eq!(granted.visible_projects(MEMBER_A), BTreeSet::from(["alpha".to_string()]));
        // Revocation cuts both directions.
        let revoked = boundary(AgentSecurityConfig::default());
        assert!(!revoked.may_see(MEMBER_A, SOLO));
        assert!(!revoked.may_message(MEMBER_A, SOLO));
        assert!(!revoked.may_message(SOLO, MEMBER_A));
        // Host mode: everything as before, grants are irrelevant.
        let host = boundary(AgentSecurityConfig {
            cross_project: CrossProjectAccess::Host,
            external_agents: vec![ExternalAgentGrant {
                project: "alpha".into(),
                agent: SOLO.into(),
            }],
            ..AgentSecurityConfig::default()
        });
        assert!(host.may_see(SOLO, MEMBER_A));
        assert!(!host.external_grant(MEMBER_A, SOLO));
    }

    #[test]
    fn creator_cycles_terminate() {
        let creator_of = [(SOLO, SOLO_CHILD), (SOLO_CHILD, SOLO)]
            .into_iter()
            .map(|(s, c)| (s.to_string(), c.to_string()))
            .collect();
        let b = AgentBoundary::new(&AgentSecurityConfig::default(), HashMap::new(), creator_of);
        let _ = b.domain(SOLO);
        assert!(!b.may_interact(SOLO, OTHER_SOLO));
    }
}
