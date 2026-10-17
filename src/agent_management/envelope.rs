//! Permission envelope: what an agent may hand to an agent it creates
//! (owner decision 2026-10-07: permissions(child) ⊆ permissions(creator)).
//!
//! A child is never more permissive than its creator:
//! - sandbox: `read-only` < `workspace-write` < `danger-full-access`;
//! - approval policy no less strict: `never` < `on-request` < `untrusted`;
//! - network access only when the creator has it;
//! - a `workspace-write` child's cwd and writable roots inside the creator's
//!   writable roots (its launch cwd plus extra roots), unless the creator has
//!   full access.
//!
//! [`clamp_or_refuse`] fills unspecified fields from the creator and refuses
//! (never silently narrows or widens) anything outside the envelope, with a
//! model-facing `permission_exceeds_creator` error naming each field. It is
//! the one check for every agent-initiated launch path: Agent Management
//! create / replace today, `agent_start` later.

use std::path::{Component, Path, Prefix};

use serde::Serialize;
use serde_json::{json, Value};

use crate::config::new_thread::LaunchPermissions;
use crate::session::model::CutexSessionRecord;

/// Sandbox permissiveness.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SandboxLevel {
    ReadOnly,
    WorkspaceWrite,
    FullAccess,
}

impl SandboxLevel {
    /// Sandbox modes and the permission aliases cutex accepts for them.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "read-only" | ":read-only" | "readonly" | "read" => Some(Self::ReadOnly),
            "workspace-write" | "workspace" | ":workspace" | "cutex-workspace" => {
                Some(Self::WorkspaceWrite)
            }
            "danger-full-access" | ":danger-full-access" | "full-access" | "danger" => {
                Some(Self::FullAccess)
            }
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::FullAccess => "danger-full-access",
        }
    }
}

/// Approval strictness (higher asks the human more).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ApprovalStrictness {
    Never,
    OnRequest,
    Untrusted,
}

impl ApprovalStrictness {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "never" => Some(Self::Never),
            // `on-failure` is a deprecated alias of `on-request`.
            "on-request" | "on-failure" => Some(Self::OnRequest),
            "untrusted" => Some(Self::Untrusted),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::OnRequest => "on-request",
            Self::Untrusted => "untrusted",
        }
    }
}

/// The effective launch permissions of an existing agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionEnvelope {
    pub sandbox: SandboxLevel,
    pub approval: ApprovalStrictness,
    pub network_access: bool,
    /// Writable roots, including the launch cwd, for `workspace-write`;
    /// empty otherwise (read-only writes nothing, full access is unbounded).
    pub writable_roots: Vec<String>,
}

impl PermissionEnvelope {
    /// The envelope of a durable session from its persisted launch settings.
    pub fn from_record(record: &CutexSessionRecord) -> Result<Self, String> {
        let (sandbox, approval) = crate::runtime::args::effective_runtime_permission_defaults(record);
        let sandbox = sandbox
            .as_deref()
            .and_then(SandboxLevel::parse)
            .ok_or("the creating agent has no explicit sandbox; set its permissions first")?;
        let approval = approval
            .as_deref()
            .and_then(ApprovalStrictness::parse)
            .ok_or("the creating agent has no explicit approval policy; set its permissions first")?;
        let network_access = match sandbox {
            SandboxLevel::FullAccess => true,
            SandboxLevel::WorkspaceWrite => record.sandbox_network_access.unwrap_or(false),
            SandboxLevel::ReadOnly => false,
        };
        let writable_roots = if sandbox == SandboxLevel::WorkspaceWrite {
            std::iter::once(crate::session::service::cutex_session_launch_cwd(record).to_string())
                .chain(record.sandbox_writable_roots.iter().cloned())
                .collect()
        } else {
            Vec::new()
        };
        Ok(Self {
            sandbox,
            approval,
            network_access,
            writable_roots,
        })
    }

    /// Running an arbitrary command outside any sandbox without asking the
    /// human is already within this envelope.
    pub fn runs_unsandboxed_without_approval(&self) -> bool {
        self.sandbox == SandboxLevel::FullAccess && self.approval == ApprovalStrictness::Never
    }

    /// Whether `path` is writable under this envelope.
    pub fn may_write(&self, path: &str) -> bool {
        match self.sandbox {
            SandboxLevel::FullAccess => true,
            SandboxLevel::ReadOnly => false,
            SandboxLevel::WorkspaceWrite => self
                .writable_roots
                .iter()
                .any(|root| path_within(path, root)),
        }
    }
}

/// What a create-like request asks for; `None` inherits the creator's value
/// (writable roots: none beyond the child's own cwd).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RequestedPermissions {
    pub sandbox: Option<String>,
    pub approval: Option<String>,
    pub network_access: Option<bool>,
    pub writable_roots: Option<Vec<String>>,
    /// The child's working directory (always a writable root for
    /// `workspace-write`).
    pub cwd: String,
}

/// One field outside the creator's envelope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EnvelopeViolation {
    pub field: &'static str,
    pub requested: Value,
    pub limit: Value,
}

/// Refusal: the request exceeds the creator's permissions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionExceedsCreator {
    pub violations: Vec<EnvelopeViolation>,
}

impl PermissionExceedsCreator {
    pub const CODE: &'static str = "permission_exceeds_creator";

    pub fn detail(&self) -> String {
        let fields = self
            .violations
            .iter()
            .map(|violation| violation.field)
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "Not created: {fields} would exceed your own permissions. A new agent can have at most your sandbox, at least your approval strictness, network only if you have it, and writable paths inside yours. Request less, or ask the owner to create it."
        )
    }

    pub fn to_json(&self) -> Value {
        json!({
            "ok": false,
            "status": "no_write",
            "code": Self::CODE,
            "detail": self.detail(),
            "fields": self.violations,
        })
    }
}

/// Fill unspecified fields from the creator and refuse anything outside its
/// envelope. Returns the child's effective launch permissions.
pub fn clamp_or_refuse(
    creator: &PermissionEnvelope,
    requested: &RequestedPermissions,
) -> Result<LaunchPermissions, PermissionExceedsCreator> {
    let mut violations = Vec::new();
    let sandbox = match requested.sandbox.as_deref() {
        None => creator.sandbox,
        Some(value) => match SandboxLevel::parse(value) {
            Some(level) => level,
            None => {
                violations.push(EnvelopeViolation {
                    field: "sandbox_mode",
                    requested: json!(value),
                    limit: json!(creator.sandbox.as_str()),
                });
                creator.sandbox
            }
        },
    };
    if sandbox > creator.sandbox {
        violations.push(EnvelopeViolation {
            field: "sandbox_mode",
            requested: json!(sandbox.as_str()),
            limit: json!(creator.sandbox.as_str()),
        });
    }
    let approval = match requested.approval.as_deref() {
        None => creator.approval,
        Some(value) => match ApprovalStrictness::parse(value) {
            Some(level) => level,
            None => {
                violations.push(EnvelopeViolation {
                    field: "approval_policy",
                    requested: json!(value),
                    limit: json!(creator.approval.as_str()),
                });
                creator.approval
            }
        },
    };
    if approval < creator.approval {
        violations.push(EnvelopeViolation {
            field: "approval_policy",
            requested: json!(approval.as_str()),
            limit: json!(creator.approval.as_str()),
        });
    }
    let network_access = requested.network_access.unwrap_or(creator.network_access)
        && sandbox == SandboxLevel::WorkspaceWrite;
    if requested.network_access == Some(true) && !creator.network_access {
        violations.push(EnvelopeViolation {
            field: "network_access",
            requested: json!(true),
            limit: json!(false),
        });
    }
    let writable_roots = requested.writable_roots.clone().unwrap_or_default();
    if sandbox == SandboxLevel::WorkspaceWrite && creator.sandbox != SandboxLevel::FullAccess {
        if !creator.may_write(&requested.cwd) {
            violations.push(EnvelopeViolation {
                field: "cwd",
                requested: json!(requested.cwd),
                limit: json!(creator.writable_roots),
            });
        }
        let outside: Vec<&String> = writable_roots
            .iter()
            .filter(|root| !creator.may_write(root))
            .collect();
        if !outside.is_empty() {
            violations.push(EnvelopeViolation {
                field: "writable_roots",
                requested: json!(outside),
                limit: json!(creator.writable_roots),
            });
        }
    }
    if !violations.is_empty() {
        violations.dedup_by(|a, b| a.field == b.field);
        return Err(PermissionExceedsCreator { violations });
    }
    Ok(LaunchPermissions {
        sandbox: sandbox.as_str().to_string(),
        approval: approval.as_str().to_string(),
        // Agent-created agents keep the native / profile default reviewer.
        approvals_reviewer: None,
        network_access,
        writable_roots: if sandbox == SandboxLevel::WorkspaceWrite {
            writable_roots
        } else {
            Vec::new()
        },
    })
}

/// Requested permissions of an Agent Management spec. The `permissions`
/// alias and `sandbox_mode` must both stay inside the envelope, so the more
/// permissive of the two is checked. Specs carry no network or extra roots.
pub fn requested_from_spec(spec: &crate::agent_management::ManagedAgentSpec) -> RequestedPermissions {
    let alias = SandboxLevel::parse(&spec.permissions);
    let mode = SandboxLevel::parse(&spec.sandbox_mode);
    let sandbox = match (alias, mode) {
        (Some(alias), Some(mode)) => alias.max(mode).as_str().to_string(),
        _ => spec.sandbox_mode.clone(),
    };
    RequestedPermissions {
        sandbox: Some(sandbox),
        approval: Some(spec.approval_policy.clone()),
        network_access: Some(false),
        writable_roots: Some(Vec::new()),
        cwd: spec.cwd.clone(),
    }
}

/// Lexical, case-normalized (on Windows) path components for containment.
fn normalized_components(path: &str) -> Option<Vec<String>> {
    let path = Path::new(path.trim());
    if !path.is_absolute() {
        return None;
    }
    let fold = |text: &str| {
        if cfg!(windows) {
            text.to_lowercase()
        } else {
            text.to_string()
        }
    };
    let mut parts: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => parts.push(match prefix.kind() {
                Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                    format!("{}:", (letter as char).to_ascii_lowercase())
                }
                Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => fold(&format!(
                    "//{}/{}",
                    server.to_string_lossy(),
                    share.to_string_lossy()
                )),
                _ => fold(&prefix.as_os_str().to_string_lossy()),
            }),
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                // Never climb above the prefix/root.
                if parts.len() > usize::from(cfg!(windows)) {
                    parts.pop();
                }
            }
            Component::Normal(part) => parts.push(fold(&part.to_string_lossy())),
        }
    }
    Some(parts)
}

/// Whether `path` is `root` or inside it (lexically; both absolute).
pub fn path_within(path: &str, root: &str) -> bool {
    match (normalized_components(path), normalized_components(root)) {
        (Some(path), Some(root)) => path.len() >= root.len() && path[..root.len()] == root[..],
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abs(path: &str) -> String {
        if cfg!(windows) {
            format!("C:{}", path.replace('/', "\\"))
        } else {
            path.to_string()
        }
    }

    fn workspace_creator(network: bool) -> PermissionEnvelope {
        PermissionEnvelope {
            sandbox: SandboxLevel::WorkspaceWrite,
            approval: ApprovalStrictness::OnRequest,
            network_access: network,
            writable_roots: vec![abs("/work/project"), abs("/work/shared")],
        }
    }

    fn request(sandbox: &str, approval: &str, cwd: &str) -> RequestedPermissions {
        RequestedPermissions {
            sandbox: Some(sandbox.into()),
            approval: Some(approval.into()),
            network_access: None,
            writable_roots: None,
            cwd: abs(cwd),
        }
    }

    #[test]
    fn child_within_envelope_is_accepted_and_inherits_unspecified_fields() {
        let creator = workspace_creator(true);
        let child = clamp_or_refuse(&creator, &request("workspace-write", "untrusted", "/work/project/.agents/a"))
            .unwrap();
        assert_eq!(child.sandbox, "workspace-write");
        assert_eq!(child.approval, "untrusted");
        assert!(child.network_access);
        let inherited = clamp_or_refuse(
            &creator,
            &RequestedPermissions { cwd: abs("/work/shared/x"), ..RequestedPermissions::default() },
        )
        .unwrap();
        assert_eq!(inherited.sandbox, "workspace-write");
        assert_eq!(inherited.approval, "on-request");
        let read_only = clamp_or_refuse(&creator, &request("read-only", "on-request", "/elsewhere"))
            .unwrap();
        assert_eq!(read_only.sandbox, "read-only");
        assert!(!read_only.network_access);
    }

    #[test]
    fn every_widening_is_refused_with_the_offending_fields() {
        let creator = workspace_creator(false);
        let error = clamp_or_refuse(&creator, &request("danger-full-access", "never", "/work/project"))
            .unwrap_err();
        let fields: Vec<_> = error.violations.iter().map(|v| v.field).collect();
        assert_eq!(fields, vec!["sandbox_mode", "approval_policy"]);
        assert_eq!(error.to_json()["code"], "permission_exceeds_creator");
        assert_eq!(error.to_json()["fields"][0]["limit"], "workspace-write");

        let mut networked = request("workspace-write", "on-request", "/work/project/sub");
        networked.network_access = Some(true);
        let error = clamp_or_refuse(&creator, &networked).unwrap_err();
        assert_eq!(error.violations[0].field, "network_access");

        let error = clamp_or_refuse(&creator, &request("workspace-write", "on-request", "/work/projectile"))
            .unwrap_err();
        assert_eq!(error.violations[0].field, "cwd");
        let error = clamp_or_refuse(&creator, &request("workspace-write", "on-request", "/work/project/../other"))
            .unwrap_err();
        assert_eq!(error.violations[0].field, "cwd");

        let mut rooted = request("workspace-write", "on-request", "/work/project");
        rooted.writable_roots = Some(vec![abs("/work/shared/ok"), abs("/etc")]);
        let error = clamp_or_refuse(&creator, &rooted).unwrap_err();
        assert_eq!(error.violations[0].field, "writable_roots");
        assert_eq!(error.violations[0].requested, json!([abs("/etc")]));

        let error = clamp_or_refuse(&creator, &request("sideways", "sometimes", "/work/project"))
            .unwrap_err();
        assert_eq!(error.violations.len(), 2);
    }

    #[test]
    fn full_access_creators_bound_only_sandbox_and_approval() {
        let creator = PermissionEnvelope {
            sandbox: SandboxLevel::FullAccess,
            approval: ApprovalStrictness::Never,
            network_access: true,
            writable_roots: Vec::new(),
        };
        assert!(creator.runs_unsandboxed_without_approval());
        let child = clamp_or_refuse(&creator, &request("workspace-write", "never", "/anywhere")).unwrap();
        assert_eq!(child.sandbox, "workspace-write");
        clamp_or_refuse(&creator, &request("danger-full-access", "never", "/anywhere")).unwrap();
        let read_only_creator = PermissionEnvelope {
            sandbox: SandboxLevel::ReadOnly,
            approval: ApprovalStrictness::OnRequest,
            network_access: false,
            writable_roots: Vec::new(),
        };
        assert!(!read_only_creator.runs_unsandboxed_without_approval());
        assert!(clamp_or_refuse(&read_only_creator, &request("workspace-write", "on-request", "/x")).is_err());
    }

    #[test]
    fn envelope_from_record_uses_persisted_launch_permissions() {
        let mut record = CutexSessionRecord::new(
            "cutex.00000000-0000-0000-0000-000000000001".into(),
            None,
            "host".into(),
            abs("/work/project"),
            None,
        )
        .unwrap();
        assert!(PermissionEnvelope::from_record(&record).is_err());
        LaunchPermissions {
            sandbox: "workspace-write".into(),
            approval: "on-request".into(),
            approvals_reviewer: None,
            network_access: true,
            writable_roots: vec![abs("/work/shared")],
        }
        .apply_to_record(&mut record);
        let envelope = PermissionEnvelope::from_record(&record).unwrap();
        assert_eq!(envelope.sandbox, SandboxLevel::WorkspaceWrite);
        assert!(envelope.network_access);
        assert_eq!(envelope.writable_roots, vec![abs("/work/project"), abs("/work/shared")]);
        record.permission_defaults = Some("full-access".into());
        record.sandbox_mode = None;
        record.approval_policy = None;
        let full = PermissionEnvelope::from_record(&record).unwrap();
        assert!(full.runs_unsandboxed_without_approval());
    }

    #[test]
    fn path_containment_is_lexical_and_component_wise() {
        assert!(path_within(&abs("/a/b/c"), &abs("/a/b")));
        assert!(path_within(&abs("/a/b"), &abs("/a/b")));
        assert!(!path_within(&abs("/a/bc"), &abs("/a/b")));
        assert!(!path_within(&abs("/a/b/../c"), &abs("/a/b")));
        assert!(!path_within("relative/path", &abs("/a")));
        if cfg!(windows) {
            assert!(path_within(r"\\?\C:\Work\Project\x", r"c:\work\project"));
        }
    }
}
