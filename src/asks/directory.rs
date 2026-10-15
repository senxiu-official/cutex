//! A per-call view of the agents asks can address: names, profiles,
//! domains and the boundary (asks §4.2, §6.1). Loaded from the session
//! store and the Agent Management store; tests build it directly.

use std::collections::BTreeMap;

use crate::agent_bus::boundary::AgentBoundary;
use crate::config::agent_security::DomainKey;
use crate::session::model::CutexSessionRuntimeBackend;

use super::store::AskError;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentInfo {
    pub cutex_session_id: String,
    pub name: Option<String>,
    pub profile: Option<String>,
    pub model: Option<String>,
    pub project: Option<String>,
    pub archived: bool,
    pub agent_enabled: bool,
    pub claude: bool,
    pub runtime_generation: u64,
    pub thread_id: Option<String>,
    pub note: Option<String>,
}

impl AgentInfo {
    pub fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.cutex_session_id.clone())
    }
}

#[derive(Clone, Debug, Default)]
pub struct Directory {
    pub agents: BTreeMap<String, AgentInfo>,
    pub boundary: AgentBoundary,
}

impl Directory {
    pub fn load() -> Result<Self, AskError> {
        let boundary = AgentBoundary::load()
            .map_err(|error| AskError::new("persistence_unavailable", format!("boundary: {error:#}")))?;
        // Borrow the cached store (the scheduler ticks every second; a clone
        // of the whole store per tick was most of its idle cost).
        let agents = crate::session::store::with_cutex_session_store(|store| {
            store
                .sessions
                .values()
                .map(|record| {
                    (
                        record.cutex_session_id.clone(),
                        AgentInfo {
                            cutex_session_id: record.cutex_session_id.clone(),
                            name: record.formal_agent_name.clone().or_else(|| record.display_name_override.clone()),
                            profile: record.profile.clone(),
                            model: record.model_defaults.clone(),
                            project: boundary.project_of(&record.cutex_session_id).map(str::to_string),
                            archived: record.is_retired(),
                            agent_enabled: record.agent_enabled,
                            claude: record.runtime_backend == CutexSessionRuntimeBackend::ClaudeSubscription,
                            runtime_generation: record.runtime_generation,
                            thread_id: record.app_server_thread_id().map(str::to_string),
                            note: record.note.clone(),
                        },
                    )
                })
                .collect::<BTreeMap<_, _>>()
        })
        .map_err(|error| AskError::new("persistence_unavailable", format!("session store: {error:#}")))?;
        Ok(Self { agents, boundary })
    }

    pub fn get(&self, session: &str) -> Option<&AgentInfo> {
        self.agents.get(session)
    }

    /// Whether `caller` may address `other` with an ask: the agents it may
    /// see (mutual cooperation, or an external agent granted to its
    /// project). A granted external agent answers through the ask without
    /// seeing the project, so it cannot start asks into it.
    pub fn may_interact(&self, caller: &str, other: &str) -> bool {
        self.boundary.may_see(caller, other)
    }

    /// Same domain (not only linked): requester profile/model are shown to
    /// the target (§4.2).
    pub fn same_domain(&self, left: &str, right: &str) -> bool {
        self.boundary.host_wide() || self.boundary.domain(left) == self.boundary.domain(right)
    }

    pub fn domain(&self, session: &str) -> DomainKey {
        self.boundary.domain(session)
    }

    /// Agents `caller` may address (local, not archived, agent-enabled).
    pub fn reachable<'a>(&'a self, caller: &'a str) -> impl Iterator<Item = &'a AgentInfo> + 'a {
        self.agents
            .values()
            .filter(move |agent| !agent.archived && agent.cutex_session_id != caller && self.may_interact(caller, &agent.cutex_session_id))
    }

    /// Resolve a target by durable id or unique name within the caller's
    /// reach. Outside the boundary looks like `recipient_outside_boundary`
    /// only for exact ids (names outside the boundary are not revealed).
    pub fn resolve(&self, caller: Option<&str>, target: &str) -> Result<&AgentInfo, AskError> {
        let target = target.trim();
        if let Some(agent) = self.agents.get(target) {
            if agent.archived {
                return Err(AskError::new("recipient_archived", "that agent is archived"));
            }
            if let Some(caller) = caller {
                if !self.may_interact(caller, target) {
                    return Err(AskError::new(
                        "recipient_outside_boundary",
                        "that agent is outside your project boundary",
                    ));
                }
            }
            return Ok(agent);
        }
        // Reuse the host snapshot's sorted index. A concurrently changed
        // snapshot (or a fixture Directory) falls back to this exact view.
        // Visibility is applied before uniqueness and candidate reporting.
        let all = crate::short_ids::session_index().ok()
            .filter(|index| self.agents.values().all(|agent|index.same_entry(&agent.cutex_session_id,agent.name.as_deref())))
            .unwrap_or_else(||std::sync::Arc::new(crate::short_ids::Index::new(self.agents.values().map(|agent|crate::short_ids::Entry {id:agent.cutex_session_id.clone(),name:agent.name.clone()}))));
        let id = all.resolve_visible(target, &all, crate::short_ids::settings().minimum(), |id|self.agents.get(id).is_some_and(|agent|!agent.archived&&caller.is_none_or(|caller|self.may_interact(caller,id))))?;
        self.agents.get(&id).ok_or_else(|| AskError::new("id_not_found", "agent disappeared"))
    }

    /// Resolve a note recipient: like [`Self::resolve`], plus exact ids the
    /// caller may message without seeing them (a granted external agent
    /// writing back to the project's agents, as with the bus `send`).
    pub fn resolve_tell(&self, caller: &str, target: &str) -> Result<&AgentInfo, AskError> {
        let id = target.trim();
        if let Some(agent) = self.agents.get(id) {
            if !agent.archived && !self.may_interact(caller, id) && self.boundary.may_message(caller, id) {
                return Ok(agent);
            }
        }
        self.resolve(Some(caller), target)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::agent_security::AgentSecurityConfig;
    use std::collections::HashMap;

    pub fn agent(session: &str, name: &str) -> AgentInfo {
        AgentInfo {
            cutex_session_id: session.into(),
            name: Some(name.into()),
            profile: Some("deepseek".into()),
            model: Some("deepseek-v4".into()),
            agent_enabled: true,
            runtime_generation: 1,
            thread_id: Some(format!("thread-{name}")),
            ..AgentInfo::default()
        }
    }

    /// Two agents in project alpha, one outside.
    pub fn directory(agents: &[(&str, &str, Option<&str>)]) -> Directory {
        let project_of: HashMap<String, String> = agents
            .iter()
            .filter_map(|(session, _, project)| project.map(|project| (session.to_string(), project.to_string())))
            .collect();
        Directory {
            agents: agents
                .iter()
                .map(|(session, name, project)| {
                    let mut info = agent(session, name);
                    info.project = project.map(str::to_string);
                    (session.to_string(), info)
                })
                .collect(),
            boundary: AgentBoundary::new(&AgentSecurityConfig::default(), project_of, HashMap::new()),
        }
    }

    #[test]
    fn resolve_by_id_or_unique_name_inside_the_boundary() {
        let a = "cutex.00000000-0000-0000-0000-0000000000a1";
        let b = "cutex.00000000-0000-0000-0000-0000000000b1";
        let c = "cutex.00000000-0000-0000-0000-0000000000c1";
        let dir = directory(&[(a, "Eva", Some("alpha")), (b, "Kimi", Some("alpha")), (c, "Kimi", None)]);
        assert_eq!(dir.resolve(Some(a), "kimi").unwrap().cutex_session_id, b);
        assert_eq!(dir.resolve(Some(a), b).unwrap().name.as_deref(), Some("Kimi"));
        assert_eq!(dir.resolve(Some(a), c).unwrap_err().code, "recipient_outside_boundary");
        assert_eq!(dir.resolve(None, "Kimi").unwrap_err().code, "id_ambiguous");
        assert_eq!(dir.resolve(Some(a), "nobody").unwrap_err().code, "id_not_found");
        assert!(dir.same_domain(a, b));
        assert_eq!(dir.reachable(a).count(), 1);
    }

    /// An external agent granted to project alpha can be asked by alpha's
    /// agents; it can tell them back by id but cannot ask or list them.
    #[test]
    fn external_agent_grants_reach_asks_one_way() {
        use crate::config::agent_security::ExternalAgentGrant;
        let a = "cutex.00000000-0000-0000-0000-0000000000a1";
        let x = "cutex.00000000-0000-0000-0000-0000000000c1";
        let mut dir = directory(&[(a, "Eva", Some("alpha")), (x, "Reviewer", None)]);
        let security = AgentSecurityConfig {
            external_agents: vec![ExternalAgentGrant { project: "alpha".into(), agent: x.into() }],
            ..AgentSecurityConfig::default()
        };
        dir.boundary = AgentBoundary::new(&security, HashMap::from([(a.to_string(), "alpha".to_string())]), HashMap::new());
        assert_eq!(dir.resolve(Some(a), "reviewer").unwrap().cutex_session_id, x);
        assert_eq!(dir.reachable(a).count(), 1);
        assert_eq!(dir.resolve(Some(x), a).unwrap_err().code, "recipient_outside_boundary");
        assert_eq!(dir.reachable(x).count(), 0);
        assert_eq!(dir.resolve_tell(x, a).unwrap().cutex_session_id, a);
        assert_eq!(dir.resolve_tell(x, "eva").unwrap_err().code, "id_not_found");
        assert_eq!(dir.resolve_tell(x, "cutex.…0000a1").unwrap_err().code, "id_not_found");
    }

    #[test]
    fn short_candidates_hide_outside_and_archived_agents() {
        let a="cutex.018ab123-0000-7000-8000-0000000000a1";
        let b="cutex.018ab123-0000-7000-8000-0000000000b1";
        let hidden="cutex.018ab123-0000-7000-8000-0000000000c1";
        let retired="cutex.018ab123-0000-7000-8000-0000000000d1";
        let mut dir=directory(&[(a,"Eva",Some("alpha")),(b,"Kimi",Some("alpha")),(hidden,"Secret",None),(retired,"Old",Some("alpha"))]);
        dir.agents.get_mut(retired).unwrap().archived=true;
        let error=dir.resolve(Some(a),"018ab123").unwrap_err();
        assert_eq!(error.code,"id_ambiguous");
        let candidates=error.to_json()["candidates"].clone();
        assert_eq!(candidates.as_array().unwrap().len(),2);
        assert!(!candidates.to_string().contains(hidden));
        assert!(!candidates.to_string().contains(retired));
        assert_eq!(dir.resolve(Some(a),"cutex.…0000d1").unwrap_err().code,"id_not_found");
        assert_eq!(dir.resolve(Some(a),retired).unwrap_err().code,"recipient_archived");
    }
}

/// `project:<id>` / `agent:<root>` label of a domain (budget keys).
pub fn domain_label(domain: &DomainKey) -> String {
    match domain {
        DomainKey::Project(project) => format!("project:{project}"),
        DomainKey::Agent(agent) => format!("agent:{agent}"),
    }
}
