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
    fn creation_registered(record: &crate::session::model::CutexSessionRecord, roster: &crate::agent_management::AgentManagementSnapshot) -> bool {
        record.creation_action.is_none() || roster.agent_by_session(&record.cutex_session_id).is_some()
    }
    fn target_archived(record:&crate::session::model::CutexSessionRecord,roster:&crate::agent_management::AgentManagementSnapshot)->bool {
        record.is_retired()||roster.agent_by_session(&record.cutex_session_id).is_some_and(|agent|agent.retired_at.is_some())
    }
    fn canonical_name_candidates(
        &self, roster: &crate::agent_management::AgentManagementSnapshot,
        visible: &std::collections::BTreeSet<&str>,
    ) -> Vec<crate::agent_names::NameCandidate> {
        self.agents.values().filter(|agent|visible.contains(agent.cutex_session_id.as_str()))
            .map(|agent| {
                let project = crate::memory::project_for_session(roster,&agent.cutex_session_id);
                crate::agent_names::NameCandidate {
                    cutex_session_id:agent.cutex_session_id.clone(),current_name:agent.label(),
                    scope:crate::agent_names::NameScope::for_project(project.as_ref().map(|project|project.as_str())),
                    archived:agent.archived,
                    retired:roster.agent_by_session(&agent.cutex_session_id).is_some_and(|agent|agent.retired_at.is_some()),
                }
            }).collect()
    }

    /// Authority callback for negotiated peer name lookup. `visible_ids` must
    /// already be filtered by the caller's current peer visibility policy.
    /// Neither errors nor rename notes may introduce any other session.
    pub fn resolve_name_for_visible(
        request: &crate::agent_names::NameResolutionRequest,
        visible_ids: &[String],
    ) -> Result<Result<crate::agent_names::NameResolution,crate::agent_names::ResolutionFailure>,AskError> {
        let path = crate::session::store::cutex_sessions_path()
            .map_err(|error|AskError::new("persistence_unavailable",format!("name ledger: {error:#}")))?;
        let visible: std::collections::BTreeSet<_> = visible_ids.iter().map(String::as_str).collect();
        // Retry a changing catalog once. Session and roster revisions fence
        // the snapshot; names never select an ID from a mixed-generation view.
        for _ in 0..2 {
            let (before,_) = crate::session::store::formal_name_claims_at(&path,request.scope.as_ref(),&request.target)
                .map_err(|error|AskError::new("persistence_unavailable",format!("name ledger: {error:#}")))?;
            let roster = Self::director_snapshot_shared()?;
            let directory = Self::load()?;
            let (revision,claims) = crate::session::store::formal_name_claims_at(&path,request.scope.as_ref(),&request.target)
                .map_err(|error|AskError::new("persistence_unavailable",format!("name ledger: {error:#}")))?;
            if before!=revision || roster.store_revision!=Self::director_snapshot_shared()?.store_revision { continue }
            let candidates = directory.canonical_name_candidates(&roster,&visible);
            return Ok(crate::agent_names::resolve(&request.target,request.scope.as_ref(),&candidates,&claims,revision));
        }
        Err(AskError::new("name_index_changed","the name directory changed during lookup; retry the same request"))
    }

    /// Roster membership and authority, independent of host-wide bus boundaries.
    pub fn director_projects(snapshot: &crate::agent_management::AgentManagementSnapshot, caller: &str) -> Vec<String> {
        let member = snapshot.agent_by_session(caller).filter(|a| a.retired_at.is_none())
            .and_then(|a| crate::agent_management::current_project_id(snapshot, a))
            .or_else(|| {
                // A retained membership override must not revive a retired managed agent.
                if snapshot.agent_by_session(caller).is_some() { return None; }
                snapshot.current_project_memberships.iter().find(|(id, _)| id.as_str() == caller)
                    .and_then(|(_, membership)| membership.project_id.clone())
            });
        if let Some(project) = member.filter(|p| crate::memory::project_is_known(snapshot, p)) {
            return vec![project.as_str().to_owned()];
        }
        snapshot.projects.values().filter(|p| p.authorized_director_session.as_str() == caller
            && crate::memory::project_is_known(snapshot, &p.project_id))
            .map(|p| p.project_id.as_str().to_owned()).collect()
    }

    pub fn director_snapshot() -> Result<crate::agent_management::AgentManagementSnapshot, AskError> {
        crate::memory::load_agent_management_directory()
            .map_err(|e| AskError::new("persistence_unavailable", format!("project authority: {e:#}")))
    }
    pub fn director_snapshot_shared()->Result<std::sync::Arc<crate::agent_management::AgentManagementSnapshot>,AskError>{
        crate::agent_management::AgentManagementStore::open_default()
            .and_then(|store|store.directory_snapshot_shared().map_err(anyhow::Error::new))
            .map_err(|e|AskError::new("persistence_unavailable",format!("project authority: {e:#}")))
    }

    pub fn resolve_director(&self, caller: &str, explicit: Option<&str>, alias: bool)
        -> Result<crate::director_address::RoleResolution, AskError> {
        self.resolve_director_in(Self::director_snapshot_shared()?.as_ref(), caller, explicit, alias)
    }

    pub fn resolve_director_in(&self, snapshot: &crate::agent_management::AgentManagementSnapshot,
        caller: &str, explicit: Option<&str>, alias: bool) -> Result<crate::director_address::RoleResolution, AskError> {
        let candidates = Self::director_projects(snapshot, caller);
        let project = match explicit {
            Some(project) => project.to_owned(),
            None if candidates.is_empty() => return Err(AskError::new("director_project_required", "Select a project explicitly with targetRef")),
            None if candidates.len() != 1 => return Err(AskError::new("director_project_ambiguous", "More than one Director project; select targetRef.projectId")),
            None => candidates[0].clone(),
        };
        let project_id = crate::agent_management::ProjectId::new(project.clone())
            .map_err(|_| AskError::new("project_not_found", "No addressable project"))?;
        if !crate::memory::project_is_known(snapshot, &project_id) {
            return Err(AskError::new("project_not_found", "No addressable project"));
        }
        let authority = snapshot.projects.get(&project_id);
        if !candidates.contains(&project) && !authority.is_some_and(|a| self.may_interact(caller, a.authorized_director_session.as_str())) {
            return Err(AskError::new("recipient_outside_boundary", "Project is outside your communication boundary"));
        }
        let authority = authority.ok_or_else(|| AskError::new("director_unassigned", "Project has no current Director"))?;
        let id = authority.authorized_director_session.as_str();
        let target = self.get(id).filter(|a| !a.archived && a.agent_enabled)
            .ok_or_else(|| AskError::new("director_unavailable", "Current Director cannot receive agent input"))?;
        self.resolve(Some(caller), id)?;
        if alias && self.agents.values().any(|a| !a.archived && a.cutex_session_id != target.cutex_session_id
            && a.name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case("director"))
            && self.may_interact(caller, &a.cutex_session_id)) {
            return Err(AskError::new("director_alias_name_conflict", "A visible agent is also named director; use targetRef for the role or its concrete id for the agent"));
        }
        Ok(crate::director_address::RoleResolution { project_id: project, target: id.to_owned(), epoch: authority.authority_epoch })
    }

    pub fn load() -> Result<Self, AskError> {
        let roster = Self::director_snapshot_shared()?;
        let boundary = AgentBoundary::load()
            .map_err(|error| AskError::new("persistence_unavailable", format!("boundary: {error:#}")))?;
        // Borrow the cached store (the scheduler ticks every second; a clone
        // of the whole store per tick was most of its idle cost).
        let agents = crate::session::store::with_cutex_session_store(|store| {
            store
                .sessions
                .values()
                .map(|record| {
                    let registered = Self::creation_registered(record,&roster);
                    (
                        record.cutex_session_id.clone(),
                        AgentInfo {
                            cutex_session_id: record.cutex_session_id.clone(),
                            name: registered.then(||record.formal_agent_name.clone().or_else(|| record.display_name_override.clone())).flatten(),
                            profile: record.profile.clone(),
                            model: record.model_defaults.clone(),
                            project: crate::memory::project_for_session(&roster,&record.cutex_session_id).map(|project|project.as_str().to_owned()),
                            archived: Self::target_archived(record,&roster),
                            agent_enabled: registered && record.agent_enabled,
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
        self.resolve_scoped(caller,target,None).map(|(agent,_)|agent)
    }

    fn resolve_linked_name(
        &self, caller: &str, target: &str,
        candidates: &[crate::agent_names::NameCandidate], claims: &[crate::agent_names::NameClaim], revision: u64,
    ) -> Result<crate::agent_names::NameResolution,crate::agent_names::ResolutionFailure> {
        use crate::agent_names::{NameCandidate,NameScope,ResolutionFailure};
        let own = self.get(caller).and_then(|agent|agent.project.as_ref())
            .map(|project|DomainKey::Project(project.clone())).unwrap_or_else(||self.boundary.domain(caller));
        // Include history scopes even when a member has since moved projects.
        // Visibility still comes only from this authorized candidate snapshot.
        let scopes: std::collections::BTreeSet<_> = candidates.iter().map(|candidate|&candidate.scope)
            .chain(claims.iter().map(|claim|&claim.scope))
            .filter(|scope|matches!(scope,NameScope::Project {project_id}
                if self.boundary.explicitly_linked(&own,&DomainKey::Project(project_id.clone()))))
            .cloned().collect();
        let mut matched = BTreeMap::<String,NameCandidate>::new();
        let mut answers = BTreeMap::new();
        for scope in scopes {
            let result = crate::agent_names::resolve(target,Some(&scope),candidates,claims,revision);
            let candidate = match &result {
                Ok(resolved)=>candidates.iter().find(|candidate|candidate.cutex_session_id==resolved.cutex_session_id).cloned(),
                Err(ResolutionFailure::Archived {target}|ResolutionFailure::Retired {target})=>Some(target.clone()),
                Err(ResolutionFailure::Ambiguous {candidates})=>{
                    for candidate in candidates {matched.insert(candidate.cutex_session_id.clone(),candidate.clone());}
                    None
                },
                Err(ResolutionFailure::NotFound)=>None,
            };
            if let Some(candidate)=candidate {
                let id=candidate.cutex_session_id.clone();matched.insert(id.clone(),candidate);
                // Two historical addresses to one identity are one match. A
                // current address to that same identity needs no rename note.
                if !answers.contains_key(&id) || result.as_ref().is_ok_and(|answer|answer.rename_notes.is_empty()) {
                    answers.insert(id,result);
                }
            }
        }
        match matched.len() {
            0=>Err(ResolutionFailure::NotFound),
            1=>answers.into_values().next().unwrap_or(Err(ResolutionFailure::NotFound)),
            _=>Err(ResolutionFailure::Ambiguous {candidates:matched.into_values().collect()}),
        }
    }

    pub fn resolve_scoped(
        &self, caller: Option<&str>, target: &str, explicit_scope: Option<&crate::agent_names::NameScope>,
    ) -> Result<(&AgentInfo,crate::agent_names::NameResolution),AskError> {
        use crate::agent_names::{NameCandidate,NameResolution,NameScope,ResolutionFailure};
        let target=target.trim();
        if let Some(agent)=self.agents.get(target) {
            let agent=self.resolve_legacy(caller,&agent.cutex_session_id)?;
            return Ok((agent,NameResolution {cutex_session_id:agent.cutex_session_id.clone(),current_name:agent.label(),
                scope:NameScope::for_project(agent.project.as_deref()),index_revision:0,rename_notes:Vec::new()}));
        }
        let caller_scope=caller.map(|caller|NameScope::for_project(self.get(caller).and_then(|agent|agent.project.as_deref())));
        let scope=explicit_scope.or(caller_scope.as_ref());
        let path=crate::session::store::cutex_sessions_path().map_err(|error|AskError::new("persistence_unavailable",format!("name ledger: {error:#}")))?;
        let claim_scope=if explicit_scope.is_some() || caller.is_none() {scope} else {None};
        let (revision,claims)=crate::session::store::formal_name_claims_at(&path,claim_scope,target)
            .map_err(|error|AskError::new("persistence_unavailable",format!("name ledger: {error:#}")))?;
        let candidates:Vec<_>=self.agents.values().filter(|agent|caller.is_none_or(|caller|self.may_interact(caller,&agent.cutex_session_id)))
            .map(|agent|NameCandidate {cutex_session_id:agent.cutex_session_id.clone(),current_name:agent.label(),
                scope:NameScope::for_project(agent.project.as_deref()),archived:agent.archived,retired:false}).collect();
        let mut named=crate::agent_names::resolve(target,scope,&candidates,&claims,revision);
        if explicit_scope.is_none() && matches!(named,Err(ResolutionFailure::NotFound)) {
            if let Some(caller)=caller {named=self.resolve_linked_name(caller,target,&candidates,&claims,revision);}
        }
        let mut matches=std::collections::BTreeMap::new();
        match named {
            Ok(resolved)=>{matches.insert(resolved.cutex_session_id.clone(),resolved);},
            Err(ResolutionFailure::Ambiguous {candidates})=>return Err(AskError::new("id_ambiguous","more than one accessible agent matches this name; use a full ID")
                .with(serde_json::json!({"candidates":candidates}))),
            Err(ResolutionFailure::Archived {..})=>return Err(AskError::new("recipient_archived","that agent is archived")),
            Err(ResolutionFailure::Retired {..})=>return Err(AskError::new("recipient_archived","that agent is permanently retired")),
            Err(ResolutionFailure::NotFound)=>{},
        }
        if crate::short_ids::query(target).valid_hex {
            let index=crate::short_ids::Index::new(candidates.iter().filter(|candidate|!candidate.archived&&!candidate.retired)
                .map(|candidate|crate::short_ids::Entry::id(candidate.cutex_session_id.clone())));
            match index.resolve(target,&index,crate::short_ids::settings().minimum()) {
                Ok(id)=>{if !matches.contains_key(&id) {
                    let resolved=crate::agent_names::resolve(&id,None,&candidates,&[],revision).map_err(|_|AskError::new("id_not_found","agent disappeared"))?;
                    matches.insert(id,resolved);
                }},
                Err(error) if error.code=="id_ambiguous"=>return Err(error),
                Err(_)=>{},
            }
        }
        if matches.len()>1 {
            let candidates:Vec<_>=candidates.into_iter().filter(|candidate|matches.contains_key(&candidate.cutex_session_id)).collect();
            return Err(AskError::new("id_ambiguous","the name and short ID match different accessible agents; use a full ID")
                .with(serde_json::json!({"candidates":candidates})));
        }
        let resolved=matches.into_values().next().ok_or_else(||AskError::new("id_not_found","no accessible agent matches this ID or name in the selected scope"))?;
        let agent=self.resolve_legacy(caller,&resolved.cutex_session_id)?;
        Ok((agent,resolved))
    }

    fn resolve_legacy(&self, caller: Option<&str>, target: &str) -> Result<&AgentInfo, AskError> {
        let target = target.trim();
        if let Some(agent) = self.agents.get(target) {
            if agent.archived {
                if let Some(hint) = crate::director_address::replacement_hint(target, caller).ok().flatten() {
                    return Err(AskError::new("recipient_replaced", "That retired Director was replaced; choose the current role explicitly, nothing was forwarded")
                        .with(serde_json::json!({"replacement":hint})));
                }
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

    pub fn resolve_tell_scoped(&self, caller: &str, target: &str, scope: Option<&crate::agent_names::NameScope>)
        -> Result<(&AgentInfo,crate::agent_names::NameResolution),AskError> {
        let id=target.trim();
        if let Some(agent)=self.agents.get(id).filter(|agent|!agent.archived&&!self.may_interact(caller,id)&&self.boundary.may_message(caller,id)) {
            return Ok((agent,crate::agent_names::NameResolution {cutex_session_id:id.into(),current_name:agent.label(),
                scope:crate::agent_names::NameScope::for_project(agent.project.as_deref()),index_revision:0,rename_notes:Vec::new()}));
        }
        self.resolve_scoped(Some(caller),target,scope)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::agent_security::AgentSecurityConfig;
    use std::collections::HashMap;

    fn linked_fixture() -> Directory {
        let mut directory=directory(&[("caller","caller",Some("own")),
            ("own-id","v5-worker",Some("own")),("blue-id","v5-worker",Some("blue")),
            ("green-id","v5-worker",Some("green")),("hidden-id","v5-worker",Some("hidden")),
            ("indirect-id","v5-worker",Some("indirect"))]);
        let security:AgentSecurityConfig=serde_json::from_value(serde_json::json!({"cross_project":"host",
            "links":[["project:own","project:blue"],["project:own","project:green"],["project:blue","project:indirect"]]})).unwrap();
        directory.boundary=AgentBoundary::new(&security,Default::default(),Default::default());
        directory
    }

    #[test]
    fn bare_name_prefers_own_then_refuses_two_direct_links_without_hidden_candidates() {
        let mut directory=linked_fixture();
        assert_eq!(directory.resolve(Some("caller"),"v5-worker").unwrap().cutex_session_id,"own-id");
        directory.agents.get_mut("own-id").unwrap().name=Some("different".into());
        let error=directory.resolve(Some("caller"),"v5-worker").unwrap_err();
        assert_eq!(error.code,"id_ambiguous");
        let ids:std::collections::BTreeSet<_>=error.extra["candidates"].as_array().unwrap().iter()
            .map(|candidate|candidate["cutexSessionId"].as_str().unwrap()).collect();
        assert_eq!(ids,std::collections::BTreeSet::from(["blue-id","green-id"]));
        assert!(error.extra["candidates"].as_array().unwrap().iter().all(|candidate|candidate["scope"]["kind"]=="project"));
    }

    #[test]
    fn explicit_name_scope_never_falls_back_and_unlinked_or_transitive_names_stay_absent() {
        let mut directory=linked_fixture();
        for id in ["own-id","green-id"] {directory.agents.get_mut(id).unwrap().name=Some("different".into());}
        assert_eq!(directory.resolve(Some("caller"),"v5-worker").unwrap().cutex_session_id,"blue-id");
        let scope=crate::agent_names::NameScope::for_project(Some("own"));
        assert_eq!(directory.resolve_scoped(Some("caller"),"v5-worker",Some(&scope)).unwrap_err().code,"id_not_found");
        directory.agents.get_mut("blue-id").unwrap().name=Some("different".into());
        assert_eq!(directory.resolve(Some("caller"),"v5-worker").unwrap_err().code,"id_not_found");
    }

    #[test]
    fn linked_history_uses_direct_scopes_deduplicates_identity_and_retains_rename_notes() {
        use crate::agent_names::{NameCandidate,NameClaim,NameScope,ResolutionFailure};
        let mut directory=linked_fixture();
        for agent in directory.agents.values_mut() {agent.name=Some(format!("current-{}",agent.cutex_session_id));}
        let visible:Vec<_>=directory.agents.values().map(|agent|NameCandidate {
            cutex_session_id:agent.cutex_session_id.clone(),current_name:agent.label(),
            scope:NameScope::for_project(agent.project.as_deref()),archived:false,retired:false}).collect();
        let claim=|project:&str,id:&str|NameClaim {scope:NameScope::for_project(Some(project)),
            normalized_name:"v5-old".into(),cutex_session_id:id.into()};
        let blue=claim("blue","blue-id");
        let resolved=directory.resolve_linked_name("caller","v5-old",&visible,&[blue.clone(),claim("hidden","hidden-id")],7).unwrap();
        assert_eq!(resolved.cutex_session_id,"blue-id");assert_eq!(resolved.rename_notes.len(),1);
        assert_eq!(resolved.rename_notes[0].current_name,"current-blue-id");
        let same=directory.resolve_linked_name("caller","v5-old",&visible,&[blue.clone(),claim("green","blue-id")],7).unwrap();
        assert_eq!(same.cutex_session_id,"blue-id");
        let hidden_filtered:Vec<_>=visible.iter().filter(|candidate|candidate.cutex_session_id!="hidden-id").cloned().collect();
        assert_eq!(directory.resolve_linked_name("caller","v5-old",&hidden_filtered,&[claim("blue","hidden-id")],7),Err(ResolutionFailure::NotFound),
            "a linked historical claim must not introduce an invisible current target");
        let Err(ResolutionFailure::Ambiguous {candidates})=directory.resolve_linked_name("caller","v5-old",&visible,&[blue,claim("green","green-id"),claim("indirect","indirect-id")],7) else {panic!("two linked historical identities must refuse")};
        assert_eq!(candidates.iter().map(|candidate|candidate.cutex_session_id.as_str()).collect::<Vec<_>>(),vec!["blue-id","green-id"]);
    }

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

    #[test]
    fn canonical_name_scopes_survive_cross_project_host_visibility() {
        use crate::agent_names::{NameScope,NameClaim,ResolutionFailure};
        use crate::agent_management::{AgentManagementSnapshot,ProjectAuthority,ProjectId};
        use crate::role_revision::{CutexSessionId,Rfc3339};
        let (a,b) = ("cutex.a","cutex.b");
        let mut dir = directory(&[(a,"Reviewer",None),(b,"Worker",None)]);
        let mut security = AgentSecurityConfig::default();
        security.cross_project = crate::config::agent_security::CrossProjectAccess::Host;
        dir.boundary = AgentBoundary::new(&security,HashMap::new(),HashMap::new());
        assert!(dir.boundary.host_wide());
        assert!(dir.boundary.project_of(a).is_none());
        let mut roster = AgentManagementSnapshot::empty();
        for (id,project) in [(a,"project-a"),(b,"project-b")] {
            let project_id = ProjectId::new(project).unwrap();
            roster.projects.insert(project_id.clone(),ProjectAuthority {
                project_id,authorized_director_session:CutexSessionId::new(id).unwrap(),
                authority_epoch:1,updated_at:Rfc3339::new("2026-11-05T00:00:00Z").unwrap(),
            });
        }
        let visible = [a,b].into_iter().collect();
        let candidates = dir.canonical_name_candidates(&roster,&visible);
        let scope_a = NameScope::for_project(Some("project-a"));
        let scope_b = NameScope::for_project(Some("project-b"));
        let claims = [NameClaim {scope:scope_a.clone(),normalized_name:"worker".into(),cutex_session_id:a.into()}];
        assert_eq!(crate::agent_names::resolve("Worker",Some(&scope_a),&candidates,&claims,1).unwrap().cutex_session_id,a);
        assert_eq!(crate::agent_names::resolve("Worker",Some(&scope_b),&candidates,&claims,1).unwrap().cutex_session_id,b);
        assert!(matches!(crate::agent_names::resolve("Worker",None,&candidates,&claims,1),Err(ResolutionFailure::Ambiguous {..})));
    }

    #[test]
    fn creation_bound_agent_is_not_name_addressable_before_roster_registration() {
        use crate::agent_management::{AgentManagementSnapshot,ManagedAgentRecord,ManagedAgentSpec};
        use crate::role_revision::{CutexSessionId,Rfc3339};
        let mut record = crate::session::model::CutexSessionRecord::new(
            "cutex.pending".into(),None,"host".into(),"/workspace".into(),None,
        ).unwrap();
        record.formal_agent_name=Some("Worker".into());
        let mut roster = AgentManagementSnapshot::empty();
        assert!(Directory::creation_registered(&record,&roster),"legacy records keep their existing behavior");
        record.creation_action=Some("human-agent:pending-create".into());
        assert!(!Directory::creation_registered(&record,&roster));
        let id = CutexSessionId::new(record.cutex_session_id.clone()).unwrap();
        roster.agents.insert(id.clone(),ManagedAgentRecord {
            project_id:None,created_by_director_session:None,created_by_operator_session:None,
            cutex_session_id:id,native_session_id:"native".into(),created_at:Rfc3339::new("2026-11-05T00:00:00Z").unwrap(),retired_at:None,
            spec:ManagedAgentSpec {
                memory_injection: true,
                name:"Worker".into(),cwd:"/workspace".into(),profile:None,runtime_backend:"app_server".into(),
                model:String::new(),reasoning:String::new(),permissions:"default".into(),approval_policy:"never".into(),sandbox_mode:"workspace-write".into(),
                groups:Vec::new(),expose_to_im:false,pin:false,
            },
        });
        assert!(Directory::creation_registered(&record,&roster));
        assert!(!Directory::target_archived(&record,&roster));
        roster.agents.get_mut(&CutexSessionId::new(record.cutex_session_id.clone()).unwrap()).unwrap().retired_at=
            Some(Rfc3339::new("2026-11-06T00:00:00Z").unwrap());
        assert!(!record.is_retired(),"simulate a stale active session projection");
        assert!(Directory::target_archived(&record,&roster),"permanent authority retirement must close all local name/ID routes");
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
        assert_eq!(dir.resolve(Some(a), "reviewer").unwrap_err().code,"id_not_found");
        assert_eq!(dir.resolve_scoped(Some(a), "reviewer",Some(&crate::agent_names::NameScope::Unassigned)).unwrap().0.cutex_session_id,x);
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

#[cfg(test)]
mod director_tests {
    use super::*;
    use crate::agent_management::{AgentManagementSnapshot, ProjectAuthority, ProjectId};
    use serde_json::json;

    const MEMBER: &str = "cutex.00000000-0000-0000-0000-0000000000a1";
    const DIRECTOR: &str = "cutex.00000000-0000-0000-0000-0000000000b1";
    const OTHER: &str = "cutex.00000000-0000-0000-0000-0000000000c1";

    fn snapshot() -> AgentManagementSnapshot {
        let mut s = AgentManagementSnapshot::empty();
        let authority: ProjectAuthority = serde_json::from_value(json!({"project_id":"alpha",
            "authorized_director_session":DIRECTOR,"authority_epoch":7,"updated_at":"2026-10-24T00:00:00Z"})).unwrap();
        s.projects.insert(ProjectId::new("alpha").unwrap(), authority);
        s.current_project_memberships.insert(crate::role_revision::CutexSessionId::new(MEMBER).unwrap(),
            serde_json::from_value(json!({"cutex_session_id":MEMBER,"project_id":"alpha","revision":1,"updated_at":"2026-10-24T00:00:00Z"})).unwrap());
        s
    }

    #[test]
    fn host_wide_boundary_does_not_erase_authoritative_project() {
        let mut d = tests::directory(&[(MEMBER, "member", None), (DIRECTOR, "chief", None)]);
        let security = crate::config::agent_security::AgentSecurityConfig { cross_project: crate::config::agent_security::CrossProjectAccess::Host, ..Default::default() };
        d.boundary = crate::agent_bus::boundary::AgentBoundary::new(&security, Default::default(), Default::default());
        let mut s = snapshot();
        let result = d.resolve_director_in(&s, MEMBER, None, true).unwrap();
        assert_eq!((result.project_id.as_str(), result.target.as_str(), result.epoch), ("alpha", DIRECTOR, 7));
        s.current_project_memberships.clear();
        assert_eq!(d.resolve_director_in(&s, MEMBER, None, true).unwrap_err().code, "director_project_required");
        assert_eq!(d.resolve_director_in(&s, MEMBER, Some("missing"), false).unwrap_err().code, "project_not_found");
    }

    #[test]
    fn conflicting_name_requires_typed_role_and_old_name_lookup_is_unchanged() {
        let d = tests::directory(&[(MEMBER, "member", Some("alpha")), (DIRECTOR, "chief", Some("alpha")), (OTHER, "Director", Some("alpha"))]);
        let s = snapshot();
        assert_eq!(d.resolve(Some(MEMBER), "director").unwrap().cutex_session_id, OTHER);
        assert_eq!(d.resolve_director_in(&s, MEMBER, None, true).unwrap_err().code, "director_alias_name_conflict");
        assert_eq!(d.resolve_director_in(&s, MEMBER, Some("alpha"), false).unwrap().target, DIRECTOR);
    }

    #[test]
    fn unassigned_unavailable_and_boundary_errors_are_distinct() {
        let mut d = tests::directory(&[(MEMBER, "member", Some("alpha")), (DIRECTOR, "chief", Some("alpha"))]);
        let mut s = snapshot();
        let authority = s.projects.remove(&ProjectId::new("alpha").unwrap()).unwrap();
        s.project_states.insert(ProjectId::new("alpha").unwrap(), serde_json::from_value(json!({"project_id":"alpha","lifecycle":"active",
            "revision":1,"created_at":"2026-10-24T00:00:00Z","updated_at":"2026-10-24T00:00:00Z"})).unwrap());
        assert_eq!(d.resolve_director_in(&s, MEMBER, None, true).unwrap_err().code, "director_unassigned");
        s.projects.insert(authority.project_id.clone(), authority);
        d.agents.get_mut(DIRECTOR).unwrap().archived = true;
        assert_eq!(d.resolve_director_in(&s, MEMBER, None, true).unwrap_err().code, "director_unavailable");
        d.agents.get_mut(DIRECTOR).unwrap().archived = false;
        d.boundary = crate::agent_bus::boundary::AgentBoundary::new(&crate::config::agent_security::AgentSecurityConfig::default(),
            std::collections::HashMap::from([(MEMBER.into(), "alpha".into()), (DIRECTOR.into(), "elsewhere".into())]), Default::default());
        assert_eq!(d.resolve_director_in(&s, MEMBER, None, false).unwrap_err().code, "recipient_outside_boundary");
    }

    #[test]
    fn multiple_director_projects_require_explicit_context() {
        let d = tests::directory(&[(DIRECTOR, "chief", None)]);
        let mut s = snapshot();
        let mut second = s.projects.values().next().unwrap().clone();
        second.project_id = ProjectId::new("beta").unwrap();
        s.projects.insert(second.project_id.clone(), second);
        assert_eq!(d.resolve_director_in(&s, DIRECTOR, None, false).unwrap_err().code, "director_project_ambiguous");
        assert_eq!(d.resolve_director_in(&s, DIRECTOR, Some("alpha"), false).unwrap().target, DIRECTOR);
    }
}
