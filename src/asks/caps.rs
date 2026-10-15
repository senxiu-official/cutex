//! Capabilities and the ask gate condition (asks design §4.4, §4.5).
//!
//! Capabilities are what an agent can do with nobody reviewing it: sandbox
//! mode, writable roots, network access, its managed-service ceiling and its
//! reach (domain + links). Approval policy is **not** a capability (it says how
//! out-of-sandbox actions are decided, not how much power the agent has), but
//! it decides the managed-service ceiling (TS §7: only `danger-full-access` +
//! `never` registers unrestricted services without the owner).
//!
//! An agent's ask needs one review when `caps(target) ⊄ caps(requester)`;
//! unknown capabilities are never a subset. The comparison is a sibling of
//! `clamp_or_refuse` (same lexical, case-insensitive root comparison).

use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::{json, Value};

use crate::agent_bus::boundary::AgentBoundary;
use crate::agent_management::envelope::{path_within, PermissionEnvelope, SandboxLevel};
use crate::config::agent_security::DomainKey;
use crate::session::model::{CutexSessionRecord, CutexSessionRuntimeBackend};

/// What an agent can do without review. `None` fields are unknown.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    /// `None`: unknown (no explicit sandbox recorded).
    pub sandbox: Option<&'static str>,
    pub network_access: bool,
    /// `workspace-write` only: launch cwd plus extra writable roots.
    pub writable_roots: Vec<String>,
    /// The agent may register managed (unsandboxed) services without the
    /// owner (`danger-full-access` + approval `never`).
    pub unrestricted_services: bool,
    /// Domains the agent may interact with; `None` = the whole host.
    pub reach: Option<Vec<String>>,
}

/// One field where the target can do more than the requester.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityDifference {
    pub field: &'static str,
    pub target: Value,
    pub requester: Value,
}

fn domain_label(domain: &DomainKey) -> String {
    match domain {
        DomainKey::Project(project) => format!("project:{project}"),
        DomainKey::Agent(agent) => format!("agent:{agent}"),
    }
}

impl Capabilities {
    /// Capabilities from an envelope and a reach.
    pub fn new(envelope: Option<&PermissionEnvelope>, reach: Option<BTreeSet<DomainKey>>) -> Self {
        Self {
            sandbox: envelope.map(|envelope| envelope.sandbox.as_str()),
            network_access: envelope.is_some_and(|envelope| envelope.network_access),
            writable_roots: envelope
                .map(|envelope| envelope.writable_roots.clone())
                .unwrap_or_default(),
            unrestricted_services: envelope
                .is_some_and(PermissionEnvelope::runs_unsandboxed_without_approval),
            reach: reach.map(|reach| reach.iter().map(domain_label).collect()),
        }
    }

    /// Capabilities of a durable session. A Claude runtime without an
    /// explicit sandbox runs with the SDK's permission mode
    /// (`bypassPermissions` by default) and counts as unknown, so asks to it
    /// from restricted requesters are gated (§7.6).
    pub fn of_record(record: &CutexSessionRecord, boundary: &AgentBoundary) -> Self {
        let envelope = PermissionEnvelope::from_record(record).ok();
        let envelope = match (&record.runtime_backend, &envelope) {
            (CutexSessionRuntimeBackend::ClaudeSubscription, _) if record.sandbox_mode.is_none() => None,
            _ => envelope,
        };
        Self::new(envelope.as_ref(), boundary.reach(&record.cutex_session_id))
    }

    fn sandbox_level(&self) -> Option<SandboxLevel> {
        self.sandbox.and_then(SandboxLevel::parse)
    }

    /// A short digest-able value (the "version" bound into approvals).
    pub fn version(&self) -> String {
        crate::approvals::request_digest(&serde_json::to_value(self).unwrap_or(Value::Null))
    }
}

/// `Ok(())` when `target ⊆ requester`; otherwise every differing field.
pub fn subset_differences(target: &Capabilities, requester: &Capabilities) -> Vec<CapabilityDifference> {
    let mut differences = Vec::new();
    let (Some(target_level), Some(requester_level)) = (target.sandbox_level(), requester.sandbox_level())
    else {
        differences.push(CapabilityDifference {
            field: "sandbox",
            target: json!(target.sandbox),
            requester: json!(requester.sandbox),
        });
        return differences;
    };
    if target_level > requester_level {
        differences.push(CapabilityDifference {
            field: "sandbox",
            target: json!(target_level.as_str()),
            requester: json!(requester_level.as_str()),
        });
    } else if target_level == SandboxLevel::WorkspaceWrite
        && requester_level == SandboxLevel::WorkspaceWrite
    {
        let outside: Vec<&String> = target
            .writable_roots
            .iter()
            .filter(|root| !requester.writable_roots.iter().any(|mine| path_within(root, mine)))
            .collect();
        if !outside.is_empty() {
            differences.push(CapabilityDifference {
                field: "writableRoots",
                target: json!(outside),
                requester: json!(requester.writable_roots),
            });
        }
    }
    if target.network_access && !requester.network_access {
        differences.push(CapabilityDifference {
            field: "networkAccess",
            target: json!(true),
            requester: json!(false),
        });
    }
    if target.unrestricted_services && !requester.unrestricted_services {
        differences.push(CapabilityDifference {
            field: "managedServices",
            target: json!("unrestricted"),
            requester: json!("owner approval"),
        });
    }
    match (&target.reach, &requester.reach) {
        (_, None) => {}
        (None, Some(mine)) => differences.push(CapabilityDifference {
            field: "reach",
            target: json!("host"),
            requester: json!(mine),
        }),
        (Some(theirs), Some(mine)) => {
            let extra: Vec<&String> = theirs.iter().filter(|domain| !mine.contains(domain)).collect();
            if !extra.is_empty() {
                differences.push(CapabilityDifference {
                    field: "reach",
                    target: json!(extra),
                    requester: json!(mine),
                });
            }
        }
    }
    differences
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_management::envelope::ApprovalStrictness;

    fn abs(path: &str) -> String {
        if cfg!(windows) {
            format!("C:{}", path.replace('/', "\\"))
        } else {
            path.to_string()
        }
    }

    fn caps(sandbox: SandboxLevel, approval: ApprovalStrictness, roots: &[&str], reach: &[&str]) -> Capabilities {
        let envelope = PermissionEnvelope {
            sandbox,
            approval,
            network_access: sandbox == SandboxLevel::FullAccess,
            writable_roots: roots.iter().map(|root| abs(root)).collect(),
        };
        let reach = reach
            .iter()
            .map(|domain| DomainKey::Project(domain.to_string()))
            .collect();
        Capabilities::new(Some(&envelope), Some(reach))
    }

    #[test]
    fn narrower_incomparable_equal_and_unknown() {
        use ApprovalStrictness::*;
        use SandboxLevel::*;
        let full = caps(FullAccess, Never, &[], &["alpha"]);
        let write_x = caps(WorkspaceWrite, OnRequest, &["/w/x"], &["alpha"]);
        let write_y = caps(WorkspaceWrite, OnRequest, &["/w/y"], &["alpha"]);
        let write_x_sub = caps(WorkspaceWrite, Untrusted, &["/w/x/sub"], &["alpha"]);
        let read = caps(ReadOnly, OnRequest, &[], &["alpha"]);
        // Strictly wider target.
        assert!(!subset_differences(&full, &write_x).is_empty());
        // Incomparable (A writes X, B writes Y).
        let diff = subset_differences(&write_y, &write_x);
        assert_eq!(diff[0].field, "writableRoots");
        // Equal and narrower targets are not gated; approval policy is not a capability.
        assert!(subset_differences(&write_x, &write_x).is_empty());
        assert!(subset_differences(&write_x_sub, &write_x).is_empty());
        assert!(subset_differences(&read, &write_x).is_empty());
        assert!(subset_differences(&write_x, &full).is_empty());
        // Unknown capabilities are never a subset.
        let unknown = Capabilities::new(None, Some(BTreeSet::new()));
        assert!(!subset_differences(&unknown, &full).is_empty());
        assert!(!subset_differences(&read, &unknown).is_empty());
        // Wider reach is a difference.
        let linked = caps(ReadOnly, OnRequest, &[], &["alpha", "beta"]);
        assert_eq!(subset_differences(&linked, &write_x)[0].field, "reach");
        // Full access with on-request approval cannot register unrestricted services.
        let full_on_request = caps(FullAccess, OnRequest, &[], &["alpha"]);
        assert_eq!(subset_differences(&full, &full_on_request)[0].field, "managedServices");
        assert!(subset_differences(&full_on_request, &full).is_empty());
        assert_ne!(full.version(), full_on_request.version());
    }
}
