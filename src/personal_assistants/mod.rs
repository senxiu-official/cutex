//! Home-host registry and Director inbox for owner-configured Personal Assistants.
pub mod agenda;
mod bindings;
pub mod calendar;
pub mod host;
mod inbox;
pub mod owner_views;
pub mod recurring;
mod store;
#[cfg(test)]
mod tests;
pub mod tools;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
pub use store::{Store, Update};

pub const CAPABILITY: &str = "personal-assistants-local-v1";
pub(crate) const CROSS_HOST_SCOPE_FEATURE: &str = "personal-assistants-cross-host-scope-v1";
pub(crate) const CROSS_HOST_SCOPE_AVAILABLE: bool = false;
pub const CHANGED_EVENT: &str = "cutex/personalAssistant/changed";

pub fn capabilities() -> serde_json::Value {
    serde_json::json!({"localDirectorRequests":true,"agenda":true,"requiresHandoffConfirmation":true,
        "ownerOperationDelegation":false,"recurringDirectorTemplates":true,"readOnlyCalendarSnapshot":true,
        "qualifiedProjectRefs":true,"crossHostProjectScope":CROSS_HOST_SCOPE_AVAILABLE})
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Tier {
    Inbox,
    #[default]
    Coordinator,
    ProjectAdmin,
    Operational,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Operation {
    LiveServices,
    KitActivation,
    Publishing,
    Destructive,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Grants {
    pub tier: Tier,
    #[serde(default)]
    pub operations: BTreeSet<Operation>,
    #[serde(default)]
    pub full_owner: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectRef {
    pub instance_id: String,
    pub project_id: String,
}
impl ProjectRef {
    fn local_project(&self) -> Result<&str> {
        require(
            self.instance_id == crate::management::instance::instance_id(),
            "destination_scope_required",
        )?;
        Ok(&self.project_id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ScopeConfig", into = "ScopeConfig")]
pub struct Config {
    pub name: String,
    pub profile: String,
    pub home_instance_id: String,
    pub project_ids: BTreeSet<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ScopeConfig {
    name: String,
    profile: String,
    home_instance_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_ids: Option<BTreeSet<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_refs: Option<BTreeSet<ProjectRef>>,
}
impl From<Config> for ScopeConfig {
    fn from(c: Config) -> Self {
        let refs = c.project_ids.iter().map(|id| c.project_ref(id)).collect();
        Self {
            name: c.name,
            profile: c.profile,
            home_instance_id: c.home_instance_id,
            project_ids: Some(c.project_ids),
            project_refs: Some(refs),
        }
    }
}
impl TryFrom<ScopeConfig> for Config {
    type Error = &'static str;
    fn try_from(c: ScopeConfig) -> std::result::Result<Self, Self::Error> {
        let ids = match c.project_refs {
            None => c.project_ids.ok_or("project scope required")?,
            Some(refs) => {
                if refs.iter().any(|r| r.instance_id != c.home_instance_id) {
                    return Err("destination scope required");
                }
                let ids = refs.into_iter().map(|r| r.project_id).collect();
                if c.project_ids.is_some_and(|old| old != ids) {
                    return Err("project scope mismatch");
                }
                ids
            }
        };
        Ok(Self {
            name: c.name,
            profile: c.profile,
            home_instance_id: c.home_instance_id,
            project_ids: ids,
        })
    }
}
impl Config {
    fn project_ref(&self, project: &str) -> ProjectRef {
        ProjectRef {
            instance_id: self.home_instance_id.clone(),
            project_id: project.into(),
        }
    }
    fn validate(&self) -> Result<()> {
        require(
            !self.name.trim().is_empty()
                && self.name.chars().count() <= 80
                && !self.name.chars().any(char::is_control),
            "invalid_name",
        )?;
        bounded(&self.profile, 256)?;
        require(
            !self.profile.chars().any(char::is_control),
            "invalid_profile",
        )?;
        bounded(&self.home_instance_id, 100)?;
        require(self.project_ids.len() <= 256, "scope_too_large")?;
        for id in &self.project_ids {
            crate::agent_management::ProjectId::new(id.as_str())
                .map_err(|_| fault("invalid_project_id"))?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Assistant {
    pub id: String,
    pub revision: u64,
    pub config: Config,
    pub grants: Grants,
    pub coordination_project_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordination_project_ref: Option<ProjectRef>,
    pub session_id: Option<String>,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    scope_revisions: Vec<ScopeRevision>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScopeRevision {
    project_ref: ProjectRef,
    since: u64,
}
impl Assistant {
    fn scope_since(&self, project: &str) -> u64 {
        self.scope_revisions
            .iter()
            .find(|entry| {
                entry.project_ref.project_id == project
                    && entry.project_ref.instance_id == self.config.home_instance_id
            })
            .map_or(self.revision, |entry| entry.since)
    }
    fn current_scope(&self, project: &str, reviewed: u64) -> bool {
        self.config.project_ids.contains(project)
            && (self.scope_since(project)..=self.revision).contains(&reviewed)
    }
    pub(crate) fn request_revision_current(&self, request: &DirectorRequest) -> bool {
        if request.occurrence.is_some() {
            self.current_scope(&request.project_id, request.assistant_revision)
        } else {
            self.revision == request.assistant_revision
        }
    }
    fn track_scope_change(&mut self, prior: &Assistant) {
        self.scope_revisions = self
            .config
            .project_ids
            .iter()
            .map(|project| {
                let continued = self.enabled == prior.enabled
                    && self.grants.tier == prior.grants.tier
                    && prior.config.project_ids.contains(project);
                ScopeRevision {
                    project_ref: self.config.project_ref(project),
                    since: if continued {
                        prior.scope_since(project)
                    } else {
                        self.revision
                    },
                }
            })
            .collect();
    }
    pub fn require_active(&self) -> Result<()> {
        require(
            self.enabled && self.session_id.is_some(),
            "assistant_disabled",
        )
    }
    fn scoped(&self, project: &str) -> Result<()> {
        self.require_active()?;
        require(
            self.config.project_ids.contains(project),
            "project_outside_scope",
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Seat {
    pub session_id: Option<String>,
    pub epoch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RequestState {
    AwaitingOwner,
    Queued,
    Accepted,
    Clarification,
    Declined,
    Completed,
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectorRequest {
    pub id: String,
    pub assistant_id: String,
    pub project_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_ref: Option<ProjectRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurrence: Option<recurring::Occurrence>,
    pub outcome: String,
    pub constraints: String,
    pub source_ref: String,
    pub director: Seat,
    pub assistant_revision: u64,
    pub state: RequestState,
    pub revision: u64,
    pub updated_at: String,
    pub detail: Option<String>,
}

#[derive(Debug)]
pub struct Fault(pub &'static str);
impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Fault {}
pub(super) fn fault(code: &'static str) -> anyhow::Error {
    Fault(code).into()
}
pub(super) fn require(ok: bool, code: &'static str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(fault(code))
    }
}
pub(super) fn bounded(value: &str, max: usize) -> Result<()> {
    require(
        !value.trim().is_empty() && value.len() <= max,
        "invalid_text",
    )
}
pub(super) fn id(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
}

pub(crate) fn error_message(code: &'static str) -> &'static str {
    match code {
        "open_request_limit" => "This assistant already has 50 requests waiting for owner review or clarification. Retry after an existing request is confirmed, declined, or cancelled.",
        "assistant_unavailable" => "Personal Assistant storage or provisioning is unavailable. Retry the same action.",
        "full_owner_native_confirmation_required" => "Granting full owner access requires native owner confirmation, which this host does not support yet.",
        _ => code,
    }
}

pub fn error_status(error: &anyhow::Error) -> (u16, &'static str) {
    if let Some(e) = error.downcast_ref::<crate::schedules::Error>() {
        return match e {
            crate::schedules::Error::NotFound => (404, "template_not_found"),
            crate::schedules::Error::Invalid(_) => (400, "invalid_arguments"),
            crate::schedules::Error::Conflict(_) => (409, "revision_conflict"),
        };
    }
    let Some(Fault(code)) = error.downcast_ref::<Fault>() else {
        return (503, "assistant_unavailable");
    };
    let status = match *code {
        "request_not_found" | "assistant_not_found" | "project_not_found" | "route_not_found" => {
            404
        }
        "assistant_required"
        | "full_owner_native_confirmation_required"
        | "director_required"
        | "assistant_disabled"
        | "project_outside_scope" => 403,
        "revision_conflict"
        | "memory_full"
        | "open_request_limit"
        | "action_conflict"
        | "invalid_transition"
        | "director_changed"
        | "director_missing"
        | "confirmation_stale"
        | "calendar_changed"
        | "destination_scope_required"
        | "project_inactive"
        | "binding_conflict"
        | "provisioning_in_progress"
        | "creation_binding_inactive"
        | "creation_binding_incomplete"
        | "profile_unavailable"
        | "home_instance_mismatch" => 409,
        _ => 400,
    };
    (status, code)
}
