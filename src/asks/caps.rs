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
use std::path::Path;

use serde::Serialize;
use serde_json::{json, Value};

use crate::agent_bus::boundary::AgentBoundary;
use crate::agent_management::envelope::{path_within, PermissionEnvelope, SandboxLevel};
use crate::config::agent_security::DomainKey;
use crate::session::model::{CutexSessionRecord, CutexSessionRuntimeBackend, CutexSessionStore};

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
    /// The agent's own cutex-assigned scratch cwd (`<cutex home>/agents-scratch/<id>`
    /// or `<cutex home>/chats/<id>`, exactly as it appears in `writable_roots`)
    /// when no other session works in it. Private to this agent: as a target
    /// it grants nothing over shared data, so the subset check ignores it
    /// ([`private_scratch_root`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_scratch: Option<String>,
    /// A Claude agent whose command execution is off (Profile / session
    /// switch): no Bash / PowerShell tool, whatever its permission mode. Its
    /// sandbox label (`danger-full-access` for 完全访问 / 自动) only names the
    /// composer preset, so without this a target that runs commands looked
    /// like a subset (r243 acceptance). Serialized only when set, so the
    /// version of every other agent stays the same.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub no_command_execution: bool,
    /// A Claude agent with neither file-editing tools nor command execution:
    /// it cannot write files at all (with commands on it can, through the
    /// shell). Serialized only when set.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub no_file_write: bool,
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
            private_scratch: None,
            no_command_execution: false,
            no_file_write: false,
        }
    }

    /// Restrict to a Claude agent's tool switches (`write`, `exec`: what its
    /// next start launches with, [`crate::runtime::claude::effective_tools`]).
    pub fn with_claude_tools(mut self, write: bool, exec: bool) -> Self {
        self.no_command_execution = !exec;
        self.no_file_write = !write && !exec;
        self
    }

    fn can_write_files(&self) -> bool {
        !self.no_file_write && self.sandbox_level().is_some_and(|level| level >= SandboxLevel::WorkspaceWrite)
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
        let caps = Self::new(envelope.as_ref(), boundary.reach(&record.cutex_session_id));
        // A Claude agent's tool switches: command execution / file editing
        // off take those powers away whatever the permission mode says.
        match (&record.runtime_backend, &record.claude_session) {
            (CutexSessionRuntimeBackend::ClaudeSubscription, Some(session)) => {
                let (write, exec) = crate::runtime::claude::effective_tools(record, session)
                    .unwrap_or((session.allow_write, session.allow_exec));
                caps.with_claude_tools(write, exec)
            }
            _ => caps,
        }
    }

    /// [`Self::of_record`] plus the agent's private scratch cwd, judged
    /// against every session in `store` and the cutex home `home`.
    pub fn of_record_in(
        record: &CutexSessionRecord,
        boundary: &AgentBoundary,
        store: &CutexSessionStore,
        home: Option<&Path>,
    ) -> Self {
        let mut caps = Self::of_record(record, boundary);
        if caps.sandbox_level() == Some(SandboxLevel::WorkspaceWrite) {
            caps.private_scratch = home.and_then(|home| private_scratch_root(record, store, home));
        }
        caps
    }

    fn sandbox_level(&self) -> Option<SandboxLevel> {
        self.sandbox.and_then(SandboxLevel::parse)
    }

    /// A short digest-able value (the "version" bound into approvals).
    pub fn version(&self) -> String {
        crate::approvals::request_digest(&serde_json::to_value(self).unwrap_or(Value::Null))
    }
}

/// Name of a directory cutex allocates for a scratch workspace:
/// `<yyyymmdd>-<8 lowercase hex>` (`human_sessions::create_private_workspace_in`).
pub(crate) fn is_allocated_scratch_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == 17
        && bytes[..8].iter().all(u8::is_ascii_digit)
        && bytes[8] == b'-'
        && bytes[9..]
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

/// The record's launch cwd when it is exactly a cutex-assigned scratch
/// workspace `<home>/agents-scratch/<id>` or `<home>/chats/<id>` (id as cutex
/// allocates it) and no other session's cwd or writable root is that
/// directory or inside it. Any other path, inside the cutex home or not, is
/// `None` and is compared as usual.
pub fn private_scratch_root(
    record: &CutexSessionRecord,
    store: &CutexSessionStore,
    home: &Path,
) -> Option<String> {
    let cwd = crate::session::service::cutex_session_launch_cwd(record);
    let path = Path::new(cwd);
    let name = path.file_name()?.to_str()?;
    let area = path.parent()?;
    let area_name = area.file_name()?.to_str()?;
    let home_text = home.to_str()?;
    let area_home = area.parent()?.to_str()?;
    let scratch_area = [
        crate::management::v2::human_agents::AGENT_SCRATCH_DIR_NAME,
        crate::management::v2::human_sessions::SCRATCH_DIR_NAME,
    ]
    .contains(&area_name);
    // Lexical, case-insensitive on Windows (same comparison as the roots).
    let same = |a: &str, b: &str| path_within(a, b) && path_within(b, a);
    if !scratch_area || !is_allocated_scratch_name(name) || !same(area_home, home_text) {
        return None;
    }
    let shared = store.sessions.values().any(|other| {
        other.cutex_session_id != record.cutex_session_id
            && std::iter::once(crate::session::service::cutex_session_launch_cwd(other))
                .chain(other.sandbox_writable_roots.iter().map(String::as_str))
                .any(|root| path_within(root, cwd))
    });
    (!shared).then(|| cwd.to_string())
}

/// `Ok(())` when `target ⊆ requester`; otherwise every differing field.
/// The target's own private scratch cwd is not compared: it gives the target
/// no write access to anything the requester could share with it.
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
            .filter(|root| target.private_scratch.as_deref() != Some(root.as_str()))
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
    // A Claude requester without command execution: a target that can run
    // commands (every Codex agent, a Claude agent with execution on) can do
    // more, whatever both sandbox labels say.
    if requester.no_command_execution && !target.no_command_execution {
        differences.push(CapabilityDifference {
            field: "commandExecution",
            target: json!(true),
            requester: json!(false),
        });
    }
    if requester.no_file_write && target.can_write_files() {
        differences.push(CapabilityDifference {
            field: "fileWrite",
            target: json!(true),
            requester: json!(false),
        });
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

    /// r243 acceptance: a Claude requester with command execution off (完全访问 / 自动 record
    /// `danger-full-access`) delegated to a Codex agent in workspace-write that runs commands:
    /// it went straight to `queued`.
    #[test]
    fn claude_tool_switches_are_capabilities() {
        use ApprovalStrictness::*;
        use SandboxLevel::*;
        let codex_write = caps(WorkspaceWrite, OnRequest, &["/w/x"], &["alpha"]);
        let codex_read = caps(ReadOnly, OnRequest, &[], &["alpha"]);
        let claude = |write: bool, exec: bool| caps(FullAccess, Never, &[], &["alpha"]).with_claude_tools(write, exec);
        // Requester without execution: any target that runs commands is gated.
        let diff = subset_differences(&codex_write, &claude(true, false));
        assert_eq!(diff.len(), 1);
        assert_eq!((diff[0].field, &diff[0].target, &diff[0].requester), ("commandExecution", &json!(true), &json!(false)));
        assert_eq!(subset_differences(&codex_read, &claude(true, false))[0].field, "commandExecution");
        assert_eq!(subset_differences(&claude(true, true), &claude(true, false))[0].field, "commandExecution");
        // Neither editing nor execution: a writing target differs in both, a read-only one only in execution.
        let fields = |d: Vec<CapabilityDifference>| d.iter().map(|d| d.field).collect::<Vec<_>>();
        assert_eq!(fields(subset_differences(&codex_write, &claude(false, false))), ["commandExecution", "fileWrite"]);
        assert_eq!(fields(subset_differences(&codex_read, &claude(false, false))), ["commandExecution"]);
        assert!(subset_differences(&claude(false, false), &claude(false, false)).is_empty());
        // Editing off but execution on still writes through the shell: only the switch itself compares.
        assert!(subset_differences(&codex_write, &claude(false, true)).is_empty());
        // Same switches, or a target that can do less: not gated.
        assert!(subset_differences(&claude(true, false), &claude(true, false)).is_empty());
        assert!(subset_differences(&claude(true, false), &claude(true, true)).is_empty());
        assert!(subset_differences(&codex_write, &claude(true, true)).is_empty());
        // Earlier design kept: a Claude target with full access is wider than a workspace-write Codex
        // requester, even with its own execution off.
        assert_eq!(subset_differences(&claude(true, false), &codex_write)[0].field, "sandbox");
        // Every other agent's version is unchanged (the fields are serialized only when set).
        assert_eq!(serde_json::to_value(&codex_write).unwrap().get("noCommandExecution"), None);
        assert_ne!(claude(true, false).version(), claude(true, true).version());
    }

    #[test]
    fn a_claude_record_takes_its_session_tool_switches() {
        let boundary = AgentBoundary::new(
            &Default::default(),
            [("cutex.c".to_string(), "proj".to_string()), ("cutex.x".to_string(), "proj".to_string())].into_iter().collect(),
            Default::default(),
        );
        let mut claude = record("cutex.c", "/w/x", &[]);
        claude.sandbox_mode = Some("danger-full-access".into());
        claude.approval_policy = Some("never".into());
        claude.runtime_backend = CutexSessionRuntimeBackend::ClaudeSubscription;
        claude.profile = None;
        let session = |write: bool, exec: bool| crate::runtime::claude::ClaudeSession {
            deployment: crate::runtime::claude::ClaudeDeployment {
                python: "/python".into(),
                adapter_source: "/adapter".into(),
                claude_cli: "/claude".into(),
                claude_home: "/claude-home".into(),
                tui_binary: "/claude-tui".into(),
            },
            thread_id: "t".into(),
            state_dir: "/state".into(),
            model: "sonnet".into(),
            effort: None,
            instructions: String::new(),
            allow_write: write,
            allow_exec: exec,
            write_explicit: true,
            exec_explicit: true,
            native_session_id: None,
            connection_id: None,
            initial_permission_mode: None,
        };
        claude.claude_session = Some(session(true, false));
        let caps = Capabilities::of_record(&claude, &boundary);
        assert!(caps.no_command_execution && !caps.no_file_write);
        let codex = record("cutex.x", "/w/x", &[]);
        assert_eq!(subset_differences(&Capabilities::of_record(&codex, &boundary), &caps)[0].field, "commandExecution");
        claude.claude_session = Some(session(true, true));
        let caps = Capabilities::of_record(&claude, &boundary);
        assert!(!caps.no_command_execution && !caps.no_file_write);
        let diff = subset_differences(&Capabilities::of_record(&codex, &boundary), &caps);
        assert!(diff.is_empty(), "{diff:?}");
    }

    fn record(id: &str, cwd: &str, roots: &[&str]) -> CutexSessionRecord {
        serde_json::from_value(json!({
            "cutex_session_id": id,
            "host_id": "fixture",
            "cwd": abs(cwd),
            "created_at": "2026-10-17T00:00:00Z",
            "updated_at": "2026-10-17T00:00:00Z",
            "sandbox_mode": "workspace-write",
            "approval_policy": "on-request",
            "sandbox_writable_roots": roots.iter().map(|root| abs(root)).collect::<Vec<_>>(),
        }))
        .unwrap()
    }

    fn store_of(records: &[&CutexSessionRecord]) -> CutexSessionStore {
        let mut store = CutexSessionStore::default();
        for record in records {
            store.sessions.insert(record.cutex_session_id.clone(), (*record).clone());
        }
        store
    }

    const HOME: &str = "/h/.cutex";

    /// Gate between two cwd-less agents (cutex scratch cwds): the target's own
    /// scratch dir is not compared, in both directions; everything else is.
    #[test]
    fn own_scratch_cwd_is_not_compared_but_everything_else_is() {
        let home = abs(HOME);
        let home = Path::new(&home);
        // All of them members of one project (same reach).
        let boundary = AgentBoundary::new(
            &Default::default(),
            ["cutex.a", "cutex.b", "cutex.w", "cutex.p", "cutex.t", "cutex.s", "cutex.u"]
                .into_iter()
                .map(|id| (id.to_string(), "proj".to_string()))
                .collect(),
            Default::default(),
        );
        let a = record("cutex.a", "/h/.cutex/agents-scratch/20261017-0a1b2c3d", &["/w/shared"]);
        let b = record("cutex.b", "/h/.cutex/chats/20261017-1234abcd", &["/w/shared"]);
        let store = store_of(&[&a, &b]);
        let caps_of = |record: &CutexSessionRecord, store: &CutexSessionStore| {
            Capabilities::of_record_in(record, &boundary, store, Some(home))
        };
        let (caps_a, caps_b) = (caps_of(&a, &store), caps_of(&b, &store));
        assert_eq!(caps_a.private_scratch.as_deref(), Some(abs("/h/.cutex/agents-scratch/20261017-0a1b2c3d").as_str()));
        assert!(subset_differences(&caps_b, &caps_a).is_empty(), "a -> b");
        assert!(subset_differences(&caps_a, &caps_b).is_empty(), "b -> a");
        // Without the cutex home the old rule applies (each gates the other).
        let plain_b = Capabilities::of_record_in(&b, &boundary, &store, None);
        assert_eq!(subset_differences(&plain_b, &caps_a)[0].field, "writableRoots");

        // The target's other roots are still compared; only the scratch dir is dropped.
        let wider = record("cutex.w", "/h/.cutex/agents-scratch/20261017-00ff00ff", &["/w/shared", "/w/private"]);
        let store = store_of(&[&a, &wider]);
        let diff = subset_differences(&caps_of(&wider, &store), &caps_of(&a, &store));
        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0].field, "writableRoots");
        assert_eq!(diff[0].target, json!([abs("/w/private")]));

        // A requester's own scratch dir gives it nothing over a target that
        // works in an ordinary directory.
        let worker = record("cutex.p", "/w/project", &[]);
        let store = store_of(&[&a, &worker]);
        assert_eq!(subset_differences(&caps_of(&worker, &store), &caps_of(&a, &store))[0].field, "writableRoots");

        // Only the exact cutex-assigned path counts: other paths in the cutex
        // home (the area itself, a subdirectory, another area, a name cutex
        // never allocates) and a scratch-like path under another home are compared.
        for cwd in [
            "/h/.cutex/agents-scratch",
            "/h/.cutex/agents-scratch/20261017-99999999/sub",
            "/h/.cutex/profiles/20261017-0a1b2c3d",
            "/h/.cutex/agents-scratch/forum",
            "/h/.cutex/chats/20261017-0A1B2C3D",
            "/h/.cutex",
            "/other/.cutex/agents-scratch/20261017-0a1b2c3d",
        ] {
            let target = record("cutex.t", cwd, &["/w/shared"]);
            let store = store_of(&[&a, &target]);
            let caps = caps_of(&target, &store);
            assert_eq!(caps.private_scratch, None, "{cwd}");
            assert_eq!(subset_differences(&caps, &caps_of(&a, &store))[0].field, "writableRoots", "{cwd}");
        }

        // A scratch dir another session also works in (its cwd or a writable
        // root at or inside it) is not private to the target.
        for (cwd, roots) in [
            ("/h/.cutex/chats/20261017-1234abcd", &[][..]),
            ("/elsewhere", &["/h/.cutex/chats/20261017-1234abcd/out"][..]),
        ] {
            let sharer = record("cutex.s", cwd, roots);
            let store = store_of(&[&a, &b, &sharer]);
            let caps = caps_of(&b, &store);
            assert_eq!(caps.private_scratch, None, "{cwd}");
            assert_eq!(subset_differences(&caps, &caps_of(&a, &store))[0].field, "writableRoots");
        }
        // A session whose root merely contains the cutex home (e.g. the user
        // home) does not make the scratch dir shared.
        let home_agent = record("cutex.u", "/h", &[]);
        let store = store_of(&[&a, &b, &home_agent]);
        assert!(subset_differences(&caps_of(&b, &store), &caps_of(&a, &store)).is_empty());
    }
}
