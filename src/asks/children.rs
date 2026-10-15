//! Child agents: provenance, presentation and limits (asks design §8.2,
//! §8.3).
//!
//! Provenance is immutable and comes from creation records
//! (`created_by_operator_session` / `created_by_director_session` in the
//! Agent Management store): `createdBy`, `rootCreator`, `creationDepth`,
//! `originDomain`. Presentation (`displayParent`) and `lifecycleManager`
//! are mutable: the manager is the creator, and after the creator is
//! archived the project's Director (project trees) or the owner
//! (independent trees). Detaching never resets depth.
//!
//! The creation limits are checked by [`check_child_limits`]; `agent_start`
//! (not yet in P1) calls it inside its Agent Management transaction.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{json, Value};

use super::budget::AskLimits;
use super::store::AskError;

/// Upper bound when walking creator links (guards corrupt cycles).
const MAX_DEPTH: u32 = 64;

/// Facts of one agent needed for provenance.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentFacts {
    pub cutex_session_id: String,
    pub created_by: Option<String>,
    /// Current project membership (members and Directors).
    pub project: Option<String>,
    pub director: bool,
    pub archived: bool,
    pub quick_chat: bool,
    /// Started only through an owner-reviewed explicit launch (r208).
    pub explicit_launch: bool,
    /// A settled runtime binding whose process is alive.
    pub binding_live: bool,
    /// Offline explicit-launch agent whose latest launch the owner reviewed
    /// and which still matches it
    /// (`agent_management::reviewed_relaunch_receipt`).
    pub reviewed_relaunch: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Provenance {
    pub cutex_session_id: String,
    pub created_by: Option<String>,
    pub root_creator: String,
    pub creation_depth: u32,
    /// `project:<id>` or `agent:<root>`.
    pub origin_domain: String,
    pub display_parent: Option<String>,
    /// A session id, or `owner`.
    pub lifecycle_manager: String,
    /// `member` | `child` | `independent` | `quickChat`.
    pub kind: &'static str,
    /// Offline explicit-launch agent: only the owner can start it (a live
    /// binding is reconnected instead). Not part of the API body.
    #[serde(skip)]
    pub owner_start_required: bool,
    /// With `owner_start_required`: its creator or its project's Director
    /// may start it again on the owner's earlier reviewed launch
    /// (`asks::wake::decide`). Not part of the API body.
    #[serde(skip)]
    pub reviewed_relaunch: bool,
    /// The authorized Director of its project. Not part of the API body.
    #[serde(skip)]
    pub director: bool,
}

/// Provenance of every agent.
pub fn provenance(agents: &[AgentFacts]) -> BTreeMap<String, Provenance> {
    let by_id: BTreeMap<&str, &AgentFacts> = agents.iter().map(|agent| (agent.cutex_session_id.as_str(), agent)).collect();
    let director_of: BTreeMap<&str, &str> = agents
        .iter()
        .filter(|agent| agent.director)
        .filter_map(|agent| agent.project.as_deref().map(|project| (project, agent.cutex_session_id.as_str())))
        .collect();
    let mut result = BTreeMap::new();
    for agent in agents {
        let mut depth = 0;
        let mut root = agent.cutex_session_id.as_str();
        let mut origin_project = agent.project.clone();
        let mut cursor = agent;
        while let Some(creator) = cursor.created_by.as_deref() {
            if depth >= MAX_DEPTH || creator == cursor.cutex_session_id {
                break;
            }
            depth += 1;
            root = creator;
            match by_id.get(creator) {
                Some(next) => {
                    if origin_project.is_none() {
                        origin_project = next.project.clone();
                    }
                    if next.project.is_some() {
                        // A creator with standing: the tree belongs to its project.
                        break;
                    }
                    cursor = next;
                }
                None => break,
            }
        }
        let member = agent.project.is_some();
        // Members have standing: depth counts only for children.
        let creation_depth = if member { 0 } else { depth };
        let origin_domain = match &origin_project {
            Some(project) => format!("project:{project}"),
            None => format!("agent:{root}"),
        };
        let creator_archived = agent
            .created_by
            .as_deref()
            .is_some_and(|creator| by_id.get(creator).is_none_or(|creator| creator.archived));
        let lifecycle_manager = match (&agent.created_by, creator_archived, &origin_project) {
            (Some(creator), false, _) => creator.clone(),
            (Some(_), true, Some(project)) => director_of
                .get(project.as_str())
                .map(|director| director.to_string())
                .unwrap_or_else(|| "owner".into()),
            _ => "owner".into(),
        };
        let kind = if member {
            "member"
        } else if agent.quick_chat {
            "quickChat"
        } else if agent.created_by.is_some() {
            "child"
        } else {
            "independent"
        };
        result.insert(
            agent.cutex_session_id.clone(),
            Provenance {
                cutex_session_id: agent.cutex_session_id.clone(),
                created_by: agent.created_by.clone(),
                root_creator: root.to_string(),
                creation_depth,
                origin_domain,
                display_parent: if member { None } else { agent.created_by.clone() },
                lifecycle_manager,
                kind,
                owner_start_required: agent.explicit_launch && !agent.binding_live,
                reviewed_relaunch: agent.explicit_launch && !agent.binding_live && agent.reviewed_relaunch,
                director: agent.director,
            },
        );
    }
    result
}

/// Whether `creator` may create one more child now (§8.3). Refusals list
/// the existing live children so the caller can reuse one.
pub fn check_child_limits(agents: &[AgentFacts], creator: &str, limits: &AskLimits) -> Result<(), AskError> {
    let provenance = provenance(agents);
    let Some(mine) = provenance.get(creator) else {
        return Err(AskError::new("stale_runtime_identity", "the creating agent is unknown"));
    };
    let live_children = |filter: &dyn Fn(&Provenance) -> bool| -> Vec<String> {
        agents
            .iter()
            .filter(|agent| !agent.archived)
            .filter_map(|agent| provenance.get(&agent.cutex_session_id))
            .filter(|item| item.kind == "child" && filter(item))
            .map(|item| item.cutex_session_id.clone())
            .collect()
    };
    if mine.creation_depth + 1 > limits.creation_depth {
        return Err(AskError::new(
            "creation_depth_exceeded",
            format!("agents at depth {} may not create children (limit {}); ask your creator or reuse an agent", mine.creation_depth, limits.creation_depth),
        ));
    }
    let own = live_children(&|item| item.created_by.as_deref() == Some(creator));
    if own.len() >= limits.children_per_agent {
        return Err(AskError::new(
            "child_limit_reached",
            format!("you already have {} live child agents (limit {}); reuse one, or close one with agent_close", own.len(), limits.children_per_agent),
        )
        .with(json!({"children": own})));
    }
    let domain = live_children(&|item| item.origin_domain == mine.origin_domain);
    if domain.len() >= limits.children_per_domain {
        return Err(AskError::new(
            "domain_child_limit_reached",
            format!("{} already has {} live child agents (limit {})", mine.origin_domain, domain.len(), limits.children_per_domain),
        )
        .with(json!({"children": domain})));
    }
    Ok(())
}

/// Facts of every agent on this host (session store + Agent Management).
pub fn load_facts() -> anyhow::Result<Vec<AgentFacts>> {
    let snapshot = crate::memory::load_agent_management_snapshot().ok();
    let mut creator_of = BTreeMap::new();
    let mut project_of = BTreeMap::new();
    let mut directors = std::collections::BTreeSet::new();
    if let Some(snapshot) = &snapshot {
        for agent in snapshot.agents.values() {
            let session = agent.cutex_session_id.as_str().to_string();
            if let Some(creator) = agent.created_by_operator_session.as_ref().or(agent.created_by_director_session.as_ref()) {
                creator_of.insert(session.clone(), creator.as_str().to_string());
            }
            if agent.retired_at.is_none() {
                if let Some(project) = crate::agent_management::current_project_id(snapshot, agent) {
                    project_of.insert(session, project.as_str().to_string());
                }
            }
        }
        for authority in snapshot.projects.values() {
            let director = authority.authorized_director_session.as_str().to_string();
            project_of.entry(director.clone()).or_insert_with(|| authority.project_id.as_str().to_string());
            directors.insert(director);
        }
    }
    // Borrow the cached session store: this runs on every asks tick.
    crate::session::store::with_cutex_session_store(|sessions| {
        sessions
            .sessions
            .values()
            .map(|record| {
                let explicit_launch = record.explicit_launch.is_some();
                let binding_live = super::wake::runtime_binding_live(record);
                AgentFacts {
                    cutex_session_id: record.cutex_session_id.clone(),
                    created_by: creator_of.get(&record.cutex_session_id).cloned(),
                    project: project_of.get(&record.cutex_session_id).cloned(),
                    director: directors.contains(&record.cutex_session_id),
                    archived: record.is_retired(),
                    quick_chat: false,
                    explicit_launch,
                    binding_live,
                    // Only offline explicit-launch agents need it (reads the
                    // profile for the configuration comparison).
                    reviewed_relaunch: explicit_launch
                        && !binding_live
                        && crate::agent_management::reviewed_relaunch_receipt(sessions, record).is_some(),
                }
            })
            .collect()
    })
}

/// `GET /v2/asks/provenance` body.
pub fn provenance_json() -> anyhow::Result<Value> {
    let facts = load_facts()?;
    Ok(json!({ "agents": provenance(&facts).into_values().collect::<Vec<_>>() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: &str, created_by: Option<&str>, project: Option<&str>) -> AgentFacts {
        AgentFacts {
            cutex_session_id: id.into(),
            created_by: created_by.map(str::to_string),
            project: project.map(str::to_string),
            ..AgentFacts::default()
        }
    }

    /// Acceptance §12.3 item 9 (part): depth is provenance (kept when a
    /// creator is archived), an archived independent creator hands the
    /// manager role to the owner, project trees to the Director; the depth
    /// and per-agent limits refuse with the existing children listed.
    #[test]
    fn provenance_and_limits() {
        let mut director = agent("dir", None, Some("alpha"));
        director.director = true;
        let mut agents = vec![
            director,
            agent("member", Some("dir"), Some("alpha")),
            agent("c1", Some("member"), None),
            agent("g1", Some("c1"), None),
            agent("solo", None, None),
            agent("s1", Some("solo"), None),
        ];
        let p = provenance(&agents);
        assert_eq!(p["member"].kind, "member");
        assert_eq!(p["member"].creation_depth, 0);
        assert_eq!(p["c1"].creation_depth, 1);
        assert_eq!(p["g1"].creation_depth, 2);
        assert_eq!(p["g1"].origin_domain, "project:alpha");
        assert_eq!(p["s1"].origin_domain, "agent:solo");
        assert_eq!(p["solo"].kind, "independent");
        assert_eq!(p["s1"].lifecycle_manager, "solo");
        let limits = AskLimits::default();
        assert!(check_child_limits(&agents, "c1", &limits).is_ok());
        assert_eq!(check_child_limits(&agents, "g1", &limits).unwrap_err().code, "creation_depth_exceeded");
        // Archive creators: managers move.
        agents[4].archived = true;
        agents[2].archived = true;
        let p = provenance(&agents);
        assert_eq!(p["s1"].lifecycle_manager, "owner");
        assert_eq!(p["g1"].lifecycle_manager, "dir");
        assert_eq!(p["g1"].creation_depth, 2, "depth survives archival");
        // Per-agent limit.
        for n in 0..4 {
            agents.push(agent(&format!("m{n}"), Some("member"), None));
        }
        let refused = check_child_limits(&agents, "member", &limits).unwrap_err();
        assert_eq!(refused.code, "child_limit_reached");
        assert_eq!(refused.extra["children"].as_array().unwrap().len(), 4);
    }
}
