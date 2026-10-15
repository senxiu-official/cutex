//! Agent identity, group, and stable-name helpers.

use std::collections::HashSet;
use std::path::PathBuf;

use crate::profiles::model::StoredAccount;

/// Opaque proof that a completion entered through the dedicated local Job
/// Service credential. It has no wire representation and cannot be supplied
/// in an ordinary Agent Bus message body.
pub struct JobServiceSystemPrincipal {
    protocol: &'static str,
}

pub(crate) fn job_service_system_principal() -> JobServiceSystemPrincipal {
    JobServiceSystemPrincipal {
        protocol: crate::agent_bus::model::JOB_SERVICE_COMPLETION_SCHEMA,
    }
}

impl JobServiceSystemPrincipal {
    pub fn authenticate(&self) -> bool {
        self.protocol == crate::agent_bus::model::JOB_SERVICE_COMPLETION_SCHEMA
    }
}

/// Opaque proof that a message originates after Agent Management has
/// authorized and begun a service action. It has no wire representation and
/// cannot be constructed from an Agent-authored request.
pub struct AgentManagementSystemPrincipal {
    protocol: &'static str,
}

pub(crate) fn agent_management_system_principal() -> AgentManagementSystemPrincipal {
    AgentManagementSystemPrincipal {
        protocol: crate::agent_management::AGENT_MANAGEMENT_CONTRACT,
    }
}

impl AgentManagementSystemPrincipal {
    pub fn authenticate(&self) -> bool {
        self.protocol == crate::agent_management::AGENT_MANAGEMENT_CONTRACT
    }
}

pub fn account_agent_name(account: &StoredAccount) -> String {
    account
        .agent_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(account.name.as_str())
        .to_string()
}

pub fn agent_id_for_launch(account: &StoredAccount) -> String {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let project = sanitize_session_component(
        cwd.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("project"),
        24,
        "project",
    );
    let name = sanitize_session_component(&account_agent_name(account), 24, "agent");
    let hash = fnv1a_hex(format!(
        "{}\0{}\0{}\0{}",
        account.id,
        account.name,
        cwd.display(),
        std::process::id()
    ));
    format!("cutex.{name}.{project}.{}", &hash[..10])
}

pub fn merge_agent_groups(first: Vec<String>, second: Vec<String>) -> Vec<String> {
    let mut merged = first;
    merged.extend(second);
    normalize_agent_groups(merged)
}

pub fn normalize_agent_groups(groups: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut normalized = Vec::new();
    for group in groups {
        if let Some(group) = normalize_agent_group(&group) {
            if seen.insert(group.clone()) {
                normalized.push(group);
            }
        }
    }
    normalized
}

pub fn normalize_launch_agent_groups(groups: &[String]) -> Vec<String> {
    let mut normalized = normalize_agent_groups(groups.to_vec());
    let default_group = default_project_agent_group();
    if !normalized.iter().any(|group| group == &default_group) {
        normalized.insert(0, default_group);
    }
    normalized
}

pub fn normalize_agent_group(group: &str) -> Option<String> {
    let group = group.trim();
    if group.is_empty() {
        return None;
    }
    let mut out = String::new();
    let mut previous_dash = false;
    for ch in group.chars() {
        let normalized = if ch.is_ascii_alphanumeric() {
            Some(ch.to_ascii_lowercase())
        } else if matches!(ch, ':' | '.' | '_' | '-') {
            Some(ch)
        } else if ch.is_whitespace() || matches!(ch, '/' | '\\') {
            Some('-')
        } else {
            None
        };
        let Some(ch) = normalized else {
            continue;
        };
        if ch == '-' {
            if previous_dash {
                continue;
            }
            previous_dash = true;
        } else {
            previous_dash = false;
        }
        out.push(ch);
        if out.len() >= 80 {
            break;
        }
    }
    let out = out.trim_matches(|ch: char| matches!(ch, '.' | '-' | '_' | ':'));
    (!out.is_empty()).then(|| out.to_string())
}

pub fn default_project_agent_group() -> String {
    format!("project:{}", current_project_path_key())
}

pub fn current_project_path_key() -> String {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let hash = fnv1a_hex(cwd.display().to_string());
    hash[..8].to_string()
}

pub fn default_agent_group_for(path_key: Option<&str>, cwd: &str) -> String {
    let key = path_key
        .and_then(normalize_agent_group)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fnv1a_hex(cwd.to_string())[..8].to_string());
    format!("project:{key}")
}

pub fn sanitize_session_component(input: &str, max_len: usize, fallback: &str) -> String {
    let mut sanitized = String::with_capacity(max_len);
    let mut last_dash = false;

    for ch in input.chars() {
        let next = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_lowercase()
        } else if matches!(ch, '.' | '-' | '_') {
            ch
        } else {
            '-'
        };

        if next == '-' && last_dash {
            continue;
        }

        sanitized.push(next);
        last_dash = next == '-';

        if sanitized.len() >= max_len {
            break;
        }
    }

    let trimmed = sanitized.trim_matches(|ch: char| matches!(ch, '.' | '-' | '_'));
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

pub fn fnv1a_hex(input: impl AsRef<str>) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in input.as_ref().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

pub fn cutex_agent_hint() -> String {
    "Other cutex agents may be reachable through the cutex Agent Bus. Prefer the tools of the `cutex` MCP server when they are visible: `agent_list` shows only persistent cutex agents you may reach (the other members of your project; outside a project, the cutex agents you created and the ones they created), never yourself, cutex agents outside your project or native subagents you spawned; `agent_delegate` hands another agent work and brings its result back to you; `agent_tell` leaves a note that needs no answer (wake=true also starts the recipient's next turn). Use `cutex agent list` / `cutex agent send` only as a shell fallback. A note from another agent shows up in your current or next turn as a `Message Type: MESSAGE` block with a `Sender:` header; notes never start a turn by themselves. Answer with `agent_tell` and do not poll for replies; to get another agent to act, use `agent_delegate`.".to_string()
}
