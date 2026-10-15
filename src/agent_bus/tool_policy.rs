//! Which cutex MCP tools an agent is shown, by role (owner decision
//! 2026-10-07: tools a role cannot use are not listed at all).
//!
//! [`CUTEX_TOOL_RULES`] is the single table. Every tool the MCP adapter
//! defines must have a rule (a test enforces it); new tools are added here
//! with their audience. Listing is not authority: every call is still
//! authorized by the Agent Bus and its providers, so a stale list (a role
//! changed while the runtime was up) is harmless. Roles are evaluated when the
//! runtime's MCP server lists tools, i.e. at launch; restart the agent to
//! refresh after an operator grant or a Director change.

use serde::Serialize;
use serde_json::Value;

use crate::agent_management::AgentManagementSnapshot;

/// Who may see a tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Audience {
    /// Every agent.
    Everyone,
    /// Project Directors and delegated Agent Operators.
    ProjectManagers,
    /// Agents that talk to the owner: Directors and agents not created by
    /// another agent (owner decision 2026-10-08). Subject to the owner's
    /// `ownerNotify.toolAudience` knob ([`owner_notify_admits`]).
    OwnerFacing,
}

/// One row of the role → tool table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolRule {
    pub tool: &'static str,
    pub audience: Audience,
}

const fn rule(tool: &'static str, audience: Audience) -> ToolRule {
    ToolRule { tool, audience }
}

/// The role → tool table for the `cutex` MCP server.
pub const CUTEX_TOOL_RULES: &[ToolRule] = &[
    rule("browser_list", Audience::Everyone),
    rule("browser_read", Audience::Everyone),
    rule("browser_find", Audience::Everyone),
    rule("browser_act", Audience::Everyone),
    rule("browser_navigate", Audience::Everyone),
    rule("browser_screenshot", Audience::Everyone),
    rule("browser_release", Audience::Everyone),
    rule("browser_open", Audience::Everyone),
    rule("browser_claim", Audience::Everyone),
    // Delegations (委托, asks design §6.3): every role, inside the boundary.
    // delegation_wait is also subject to the profile's delegationWait.
    rule("agent_list", Audience::Everyone),
    rule("agent_delegate", Audience::Everyone),
    rule("delegation_wait", Audience::Everyone),
    rule("delegation_stop", Audience::Everyone),
    rule("delegation_close", Audience::Everyone),
    rule("delegation_read", Audience::Everyone),
    rule("delegation_progress", Audience::Everyone),
    rule("question_answer", Audience::Everyone),
    rule("agent_tell", Audience::Everyone),
    rule("delegation_await", Audience::Everyone),
    rule("delegation_resume", Audience::Everyone),
    rule("delegation_reopen", Audience::Everyone),
    // The full text of an overlength message (doc- id; message tool family).
    rule("read_overlength_content", Audience::Everyone),
    // Artifacts (results for the owner and other agents; own project and
    // own session scope).
    rule("artifact_publish", Audience::Everyone),
    rule("artifact_list", Audience::Everyone),
    rule("artifact_read", Audience::Everyone),
    // Workflows (W1): an approved script fans out to one-shot step agents.
    // Step sessions are refused server-side (one level only).
    rule("workflow_start", Audience::Everyone),
    rule("workflow_read", Audience::Everyone),
    rule("workflow_list", Audience::Everyone),
    rule("workflow_stop", Audience::Everyone),
    // Reminders.
    rule("alarm_set", Audience::Everyone),
    rule("alarm_list", Audience::Everyone),
    rule("alarm_cancel", Audience::Everyone),
    // Web services (registry; managed runtimes may need owner approval).
    rule("service_register", Audience::Everyone),
    rule("service_update", Audience::Everyone),
    rule("service_unregister", Audience::Everyone),
    rule("service_list", Audience::Everyone),
    // Search-only provider request; also subject to the profile's
    // searchTool (Codex API-key profiles, `web_search::enabled_for_record`).
    rule("web_search", Audience::Everyone),
    // Project management (members, lifecycle): Director and operators.
    rule("agent_create", Audience::ProjectManagers),
    rule("agent_archive", Audience::ProjectManagers),
    rule("member_runtime", Audience::ProjectManagers),
    rule("member_replace", Audience::ProjectManagers),
    // Agent → owner messages (docs/research/owner-notify.md).
    rule("owner_notify", Audience::OwnerFacing),
];

/// The caller's roles, as far as tool listing is concerned.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct AgentRoles {
    pub director: bool,
    pub operator: bool,
    /// Created by another agent (a Director, operator or agent): a child
    /// in the trust model, not an owner-created agent.
    #[serde(default)]
    pub agent_created: bool,
}

/// Whether `roles` are offered `owner_notify` under the owner's
/// `ownerNotify.toolAudience` (`ownerFacing` | `everyone` | `directors` |
/// `none`).
pub fn owner_notify_admits(roles: AgentRoles, audience: &str) -> bool {
    match audience {
        "everyone" => true,
        "directors" => roles.director,
        "none" => false,
        _ => roles.director || !roles.agent_created,
    }
}

/// Server-side check for an `owner_notify` call (listing is not authority).
/// Unreadable roles: refused (fail closed).
pub fn owner_notify_allowed(cutex_session_id: &str, audience: &str) -> bool {
    roles_for_session(cutex_session_id).is_ok_and(|roles| owner_notify_admits(roles, audience))
}

impl Audience {
    pub fn admits(self, roles: AgentRoles) -> bool {
        match self {
            Self::Everyone => true,
            Self::ProjectManagers => roles.director || roles.operator,
            Self::OwnerFacing => owner_notify_admits(roles, "ownerFacing"),
        }
    }
}

/// The audience of a tool name; `None` for a tool without a rule.
pub fn audience(tool: &str) -> Option<Audience> {
    CUTEX_TOOL_RULES
        .iter()
        .find(|rule| rule.tool == tool)
        .map(|rule| rule.audience)
}

/// Tool names visible to `roles`, in table order (default owner-notify
/// audience).
pub fn tool_names_for(roles: AgentRoles) -> Vec<&'static str> {
    tool_names_for_audience(roles, "ownerFacing")
}

/// Tool names visible to `roles` with the owner's `ownerNotify.toolAudience`.
pub fn tool_names_for_audience(roles: AgentRoles, owner_notify_audience: &str) -> Vec<&'static str> {
    CUTEX_TOOL_RULES
        .iter()
        .filter(|rule| match rule.audience {
            Audience::OwnerFacing => owner_notify_admits(roles, owner_notify_audience),
            audience => audience.admits(roles),
        })
        .map(|rule| rule.tool)
        .collect()
}

/// Keep only the named tools in an MCP `tools/list` result (`{"tools": [...]}`).
pub fn retain_tools(list: &mut Value, allowed: &[String]) {
    if let Some(tools) = list.get_mut("tools").and_then(Value::as_array_mut) {
        tools.retain(|tool| {
            tool.get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| allowed.iter().any(|allowed| allowed == name))
        });
    }
}

/// Drop the named tools from an MCP `tools/list` result.
pub fn retain_tools_except(list: &mut Value, removed: &[&str]) {
    if let Some(tools) = list.get_mut("tools").and_then(Value::as_array_mut) {
        tools.retain(|tool| !tool.get("name").and_then(Value::as_str).is_some_and(|name| removed.contains(&name)));
    }
}

/// The tools an owned session (the owner's quick chat) is offered: the
/// browser tools and `web_search`, each only when its profile opts in.
fn owned_session_tools(browser: bool, search: bool) -> Vec<&'static str> {
    let mut tools: Vec<&'static str> = if browser { crate::agent_bus::mcp_browser::TOOLS.to_vec() } else { Vec::new() };
    if search {
        tools.push(crate::agent_bus::web_search::TOOL);
    }
    tools
}

/// Whether a session is offered `web_search` (fails closed).
pub fn search_tool_enabled(cutex_session_id: &str) -> bool {
    session_record(cutex_session_id).is_some_and(|record| crate::agent_bus::web_search::enabled_for_record(&record))
}

/// The cutex MCP `tools/list` result for one agent: filtered by its roles, or
/// every tool when the roles cannot be read (fail open for visibility only;
/// every call is still authorised server-side).
pub fn tools_list_for_session(cutex_session_id: &str) -> Value {
    let claude = session_record(cutex_session_id).is_some_and(|record| {
        record.runtime_backend == crate::session::model::CutexSessionRuntimeBackend::ClaudeSubscription
    });
    let runtime = if claude { crate::asks::tools::ToolRuntime::Claude } else { crate::asks::tools::ToolRuntime::Codex };
    let wait = delegation_wait_enabled(cutex_session_id);
    let mut list = crate::agent_bus::mcp::tools_for(runtime, wait);
    let search = search_tool_enabled(cutex_session_id);
    if owned_session(cutex_session_id) {
        // A quick chat is no Agent Bus agent: at most the browser tools and
        // web_search.
        let allowed: Vec<String> = owned_session_tools(browser_tools_enabled(cutex_session_id), search)
            .into_iter()
            .map(str::to_string)
            .collect();
        retain_tools(&mut list, &allowed);
        return list;
    }
    if let Ok(roles) = roles_for_session(cutex_session_id) {
        let audience = crate::config::owner_notify::current().tool_audience;
        let allowed: Vec<String> = tool_names_for_audience(roles, &audience)
            .into_iter()
            .map(str::to_string)
            .collect();
        retain_tools(&mut list, &allowed);
    }
    crate::agent_bus::mcp_browser::filter_listing(&mut list, browser_tools_enabled(cutex_session_id));
    if !search {
        retain_tools_except(&mut list, &[crate::agent_bus::web_search::TOOL]);
    }
    list
}

/// Unlike legacy role visibility, browser opt-in always fails closed.
pub fn browser_tools_enabled(cutex_session_id: &str) -> bool {
    let Some(record) = session_record(cutex_session_id) else { return false; };
    browser_tools_for_record(&record)
}

pub fn browser_tools_for_record(record: &crate::session::model::CutexSessionRecord) -> bool {
    let Ok(store) = crate::profiles::settings_api::load_store() else { return false; };
    browser_tools_for_record_in(record, &store)
}

/// Sessions that may hold browser tools at all: Agent Bus agents and the
/// owner's own quick chats (owned sessions, `POST /v2/human/sessions`).
/// A quick chat gets the browser tools only, never the agent tools.
pub fn browser_eligible_session(record: &crate::session::model::CutexSessionRecord) -> bool {
    !record.is_retired() && (record.agent_enabled || record.is_owned_session())
}

/// [`browser_tools_for_record`] against an already loaded profile store:
/// an eligible session whose existing profile opted in. Missing profile,
/// unknown profile or retired session: off.
pub fn browser_tools_for_record_in(
    record: &crate::session::model::CutexSessionRecord,
    profiles: &crate::profiles::model::AccountsStore,
) -> bool {
    if !browser_eligible_session(record) { return false; }
    let Some(profile) = record.profile.as_deref() else { return false; };
    crate::profiles::settings_api::find(profiles, profile).is_some_and(crate::profiles::settings_api::browser_tools)
}

/// Whether a launch configures the `cutex` MCP server enabled: always for
/// agents; for an owned session (quick chat) only to serve its browser tools
/// or `web_search`.
pub fn cutex_mcp_enabled(record: &crate::session::model::CutexSessionRecord) -> bool {
    !record.is_owned_session()
        || browser_tools_for_record(record)
        || crate::agent_bus::web_search::enabled_for_record(record)
}

fn owned_session(cutex_session_id: &str) -> bool {
    session_record(cutex_session_id).is_some_and(|record| record.is_owned_session())
}

fn session_record(cutex_session_id: &str) -> Option<crate::session::model::CutexSessionRecord> {
    let store = crate::session::store::load_cutex_session_store_shared().ok()?;
    store.sessions.values().find(|record| record.cutex_session_id == cutex_session_id).cloned()
}

/// Whether `cutex_session_id` is offered `delegation_wait`: its profile's
/// `delegationWait`, else on for Codex and off for Claude runtimes. When the
/// stores cannot be read: on (visibility only; waiting is never required).
pub fn delegation_wait_enabled(cutex_session_id: &str) -> bool {
    let Some(record) = session_record(cutex_session_id) else {
        return true;
    };
    let claude = record.runtime_backend == crate::session::model::CutexSessionRuntimeBackend::ClaudeSubscription;
    let account = record
        .profile
        .as_deref()
        .and_then(|profile| crate::profiles::quota::find_profile(profile).ok().flatten());
    match account {
        Some(account) => crate::profiles::settings_api::delegation_wait(&account),
        None => !claude,
    }
}

/// Pure role computation from the Agent Management store. r219: a session is
/// a Director iff some project's authority names it (no Task Service seat).
pub fn roles_from(
    snapshot: Option<&AgentManagementSnapshot>,
    cutex_session_id: &str,
) -> AgentRoles {
    let is = |candidate: &crate::role_revision::CutexSessionId| candidate.as_str() == cutex_session_id;
    AgentRoles {
        director: snapshot.is_some_and(|snapshot| {
            snapshot
                .projects
                .values()
                .any(|authority| is(&authority.authorized_director_session))
        }),
        operator: snapshot.is_some_and(|snapshot| {
            snapshot
                .operator_grants
                .values()
                .any(|grants| grants.keys().any(is))
        }),
        agent_created: snapshot.is_some_and(|snapshot| {
            snapshot.agents.values().any(|agent| {
                is(&agent.cutex_session_id)
                    && (agent.created_by_operator_session.is_some() || agent.created_by_director_session.is_some())
            })
        }),
    }
}

/// Load the stores and compute the roles of one durable session.
pub fn roles_for_session(cutex_session_id: &str) -> anyhow::Result<AgentRoles> {
    let snapshot = crate::agent_management::AgentManagementStore::open_default()?
        .snapshot()
        .map_err(anyhow::Error::new)?;
    Ok(roles_from(Some(&snapshot), cutex_session_id))
}

/// Renamed tools an older facade still defines and lists when the toolset
/// allows them. The current facade never defines them (so never lists
/// them); `tools/call` still accepts them as hidden aliases. r214's
/// `document_read` (now `read_overlength_content`): drop in r216.
pub const LEGACY_ALLOWED: &[&str] = &[crate::asks::tools::DOCUMENT_READ];

/// `POST /api/mcp/v1/toolset` response body for a resolved caller.
pub fn toolset_response(cutex_session_id: &str, roles: AgentRoles) -> Value {
    let wait = delegation_wait_enabled(cutex_session_id);
    let browser = browser_tools_enabled(cutex_session_id);
    let search = search_tool_enabled(cutex_session_id);
    if owned_session(cutex_session_id) {
        // Quick chat: browser tools / web_search only (or none); no roles,
        // no legacy aliases.
        let tools = owned_session_tools(browser, search);
        return serde_json::json!({
            "ok": true,
            "cutex_session_id": cutex_session_id,
            "roles": AgentRoles::default(),
            "tools": tools,
        });
    }
    let audience = crate::config::owner_notify::current().tool_audience;
    let tools: Vec<&str> = tool_names_for_audience(roles, &audience)
        .into_iter()
        .filter(|tool| wait || *tool != crate::asks::tools::DELEGATION_WAIT)
        .filter(|tool| browser || !crate::agent_bus::mcp_browser::is_tool(tool))
        .filter(|tool| search || !crate::agent_bus::web_search::is_tool(tool))
        .chain(LEGACY_ALLOWED.iter().copied())
        .collect();
    serde_json::json!({
        "ok": true,
        "cutex_session_id": cutex_session_id,
        "roles": roles,
        "tools": tools,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_tools_missing_profile_fail_closed_for_both_runtimes() {
        assert!(!browser_tools_enabled("cutex.browser-missing"));
        for runtime in [crate::asks::tools::ToolRuntime::Codex,crate::asks::tools::ToolRuntime::Claude] {
            let mut listed=crate::agent_bus::mcp::tools_for(runtime,true);
            assert_eq!(listed["tools"].as_array().unwrap().iter().filter(|t| t["name"].as_str().is_some_and(crate::agent_bus::mcp_browser::is_tool)).count(),9);
            crate::agent_bus::mcp_browser::filter_listing(&mut listed,false);
            assert_eq!(listed["tools"].as_array().unwrap().iter().filter(|t| t["name"].as_str().is_some_and(crate::agent_bus::mcp_browser::is_tool)).count(),0);
        }
    }

    fn quick_chat_record() -> crate::session::model::CutexSessionRecord {
        let mut record = crate::session::model::CutexSessionRecord::new_at(
            "cutex.quick-browser".into(),
            Some("018e0000-0000-7000-8000-0000b0009ae6".into()),
            "host".into(),
            "/tmp".into(),
            Some("Browsing".into()),
            "2026-10-14T00:00:00Z".into(),
        )
        .unwrap();
        record.explicit_launch = Some(crate::agent_management::ExplicitLaunchContract {
            version: 999,
            migration_action_id: None,
            native_id: "018e0000-0000-7000-8000-0000b0009ae6".into(),
            native_home: "/unavailable".into(),
            bundle_manifest: "/unavailable/bundle".into(),
            bundle_sha256: crate::role_revision::Sha256::new("a".repeat(64)).unwrap(),
        });
        assert!(record.is_owned_session());
        record
    }

    fn profiles(browser: bool) -> crate::profiles::model::AccountsStore {
        let mut store = crate::profiles::model::AccountsStore::default();
        store.accounts.push(serde_json::from_value(serde_json::json!({
            "id": "browsing-id", "name": "Browsing", "browser_tools": browser,
        })).expect("minimal stored account"));
        store
    }

    #[test]
    fn quick_chats_get_browser_tools_when_their_profile_opts_in() {
        use crate::session::model::{CutexSessionArchiveState, CutexSessionRuntimeBackend};
        let on = profiles(true);
        let off = profiles(false);
        // Codex quick chat (owned explicit launch) and Claude quick chat.
        let codex = quick_chat_record();
        let mut claude = quick_chat_record();
        claude.explicit_launch = None;
        claude.runtime_backend = CutexSessionRuntimeBackend::ClaudeSubscription;
        assert!(claude.is_owned_session());
        for record in [&codex, &claude] {
            assert!(browser_eligible_session(record));
            assert!(browser_tools_for_record_in(record, &on));
            assert!(!browser_tools_for_record_in(record, &off));
            // Fail closed: missing profile, unknown profile, retired chat.
            let mut missing = record.clone();
            missing.profile = None;
            assert!(!browser_tools_for_record_in(&missing, &on));
            let mut unknown = record.clone();
            unknown.profile = Some("Deleted".into());
            assert!(!browser_tools_for_record_in(&unknown, &on));
            let mut retired = record.clone();
            retired.archive_state = CutexSessionArchiveState::Retired;
            assert!(!browser_eligible_session(&retired) && !browser_tools_for_record_in(&retired, &on));
        }
        // Agents keep the old rule; a local-only session with a formal
        // name is neither an agent nor a quick chat.
        let mut agent = codex.clone();
        agent.agent_enabled = true;
        assert!(!agent.is_owned_session() && browser_tools_for_record_in(&agent, &on));
        let mut named = codex.clone();
        named.formal_agent_name = Some("Worker".into());
        assert!(!browser_eligible_session(&named) && !browser_tools_for_record_in(&named, &on));
    }

    #[test]
    fn quick_chat_toolset_and_listing_hold_only_browser_tools() {
        use crate::config::instance::{set_test_selection, Selection};
        let previous = crate::config::instance::selected().unwrap();
        let path = std::env::temp_dir().join(format!("quick-chat-toolset-{}", uuid::Uuid::new_v4()));
        set_test_selection(Selection::Custom(path.clone()));
        let record = quick_chat_record();
        let id = record.cutex_session_id.clone();
        let mut sessions = crate::session::model::CutexSessionStore::default();
        sessions.sessions.insert(id.clone(), record.clone());
        crate::session::store::save_cutex_session_store(&sessions).unwrap();
        let browser: Vec<&str> = crate::agent_bus::mcp_browser::TOOLS.to_vec();
        let names = |list: &Value| -> Vec<String> {
            list["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
        };
        for (opted_in, expected) in [(true, browser.clone()), (false, Vec::new())] {
            crate::profiles::store::save_store(&profiles(opted_in)).unwrap();
            assert_eq!(browser_tools_enabled(&id), opted_in);
            assert_eq!(cutex_mcp_enabled(&record), opted_in, "launch enables the cutex server only for browser tools");
            let toolset = toolset_response(&id, AgentRoles::default());
            assert_eq!(toolset["tools"], serde_json::json!(expected), "Codex toolset, opt-in {opted_in}");
            // Claude descriptor (launch-time listing).
            assert_eq!(names(&tools_list_for_session(&id)), expected, "Claude listing, opt-in {opted_in}");
        }
        set_test_selection(previous);
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn web_search_is_listed_only_for_a_profile_with_search_tool() {
        use crate::config::instance::{set_test_selection, Selection};
        let previous = crate::config::instance::selected().unwrap();
        let path = std::env::temp_dir().join(format!("web-search-toolset-{}", uuid::Uuid::new_v4()));
        set_test_selection(Selection::Custom(path.clone()));
        let quick = quick_chat_record();
        let mut agent = quick.clone();
        agent.cutex_session_id = "cutex.web-search-agent".into();
        agent.agent_enabled = true;
        let mut sessions = crate::session::model::CutexSessionStore::default();
        sessions.sessions.insert(quick.cutex_session_id.clone(), quick.clone());
        sessions.sessions.insert(agent.cutex_session_id.clone(), agent.clone());
        crate::session::store::save_cutex_session_store(&sessions).unwrap();
        let names = |list: &Value| -> Vec<String> {
            list["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
        };
        for search in [true, false] {
            let mut store = crate::profiles::model::AccountsStore::default();
            store.accounts.push(serde_json::from_value(serde_json::json!({
                "id": "browsing-id", "name": "Browsing", "source": "api-key", "search_tool": search,
            })).unwrap());
            crate::profiles::store::save_store(&store).unwrap();
            assert_eq!(search_tool_enabled(&agent.cutex_session_id), search);
            // Agent: Codex toolset and the launch-time listing.
            let toolset = toolset_response(&agent.cutex_session_id, AgentRoles::default());
            let tools: Vec<&str> = toolset["tools"].as_array().unwrap().iter().map(|t| t.as_str().unwrap()).collect();
            assert_eq!(tools.contains(&"web_search"), search, "agent toolset");
            assert!(!tools.iter().any(|tool| crate::agent_bus::mcp_browser::is_tool(tool)), "browser stays off");
            assert_eq!(names(&tools_list_for_session(&agent.cutex_session_id)).contains(&"web_search".to_string()), search);
            // Quick chat: web_search alone enables the cutex MCP server.
            let quick_tools = toolset_response(&quick.cutex_session_id, AgentRoles::default());
            let expected = if search { serde_json::json!(["web_search"]) } else { serde_json::json!([]) };
            assert_eq!(quick_tools["tools"], expected);
            assert_eq!(names(&tools_list_for_session(&quick.cutex_session_id)), if search { vec!["web_search".to_string()] } else { Vec::new() });
            assert_eq!(cutex_mcp_enabled(&quick), search);
        }
        set_test_selection(previous);
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn every_defined_tool_has_exactly_one_rule() {
        let defined = crate::agent_bus::mcp::tools();
        let names: Vec<&str> = defined["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        for name in &names {
            assert_eq!(
                CUTEX_TOOL_RULES.iter().filter(|rule| rule.tool == *name).count(),
                1,
                "{name} needs exactly one rule in CUTEX_TOOL_RULES"
            );
        }
        for rule in CUTEX_TOOL_RULES {
            assert!(names.contains(&rule.tool), "rule for undefined tool {}", rule.tool);
        }
        // Renamed tools stay callable and allowed for older facades, but
        // this facade neither defines nor lists them.
        for legacy in LEGACY_ALLOWED {
            assert!(!names.contains(legacy) && audience(legacy).is_none(), "{legacy}");
            assert!(crate::asks::tools::is_tool(legacy), "{legacy} still callable");
        }
    }

    #[test]
    fn members_do_not_see_director_tools() {
        let member = tool_names_for(AgentRoles::default());
        let management = [
            "agent_create",
            "agent_archive",
            "member_runtime",
            "member_replace",
        ];
        for hidden in management {
            assert!(!member.contains(&hidden), "{hidden}");
        }
        for everyone in ["alarm_set", "alarm_list", "alarm_cancel", "service_register", "service_list"] {
            assert!(member.contains(&everyone), "{everyone}");
        }
        let operator = tool_names_for(AgentRoles { operator: true, ..AgentRoles::default() });
        for tool in management {
            assert!(operator.contains(&tool), "{tool}");
        }
        let director = tool_names_for(AgentRoles { director: true, ..AgentRoles::default() });
        for tool in management {
            assert!(director.contains(&tool), "{tool}");
        }
        // r219: director_rotate was removed; no role lists it.
        assert!(audience("director_rotate").is_none());
        assert!(!director.contains(&"director_rotate"));
    }

    #[test]
    fn retain_tools_filters_a_tools_list() {
        let mut list = crate::agent_bus::mcp::tools();
        retain_tools(&mut list, &["alarm_set".to_string(), "service_list".to_string()]);
        let names: Vec<_> = list["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["alarm_set", "service_list"]);
    }

    #[test]
    fn roles_come_from_project_authority_and_operator_grants() {
        let director = "cutex.00000000-0000-0000-0000-00000000000d";
        let operator = "cutex.00000000-0000-0000-0000-00000000000e";
        let successor = "cutex.00000000-0000-0000-0000-00000000000f";
        let member = "cutex.00000000-0000-0000-0000-000000000001";
        let snapshot: AgentManagementSnapshot = serde_json::from_value(serde_json::json!({
            "schema": "cutex/agent-management-store/v1",
            "store_revision": 1,
            "projects": {"alpha": {"project_id": "alpha", "authorized_director_session": director,
                "authority_epoch": 1, "updated_at": "2026-10-07T00:00:00Z"}},
            "operator_grants": {"alpha": {operator: {"project_id": "alpha",
                "operator_cutex_session_id": operator, "grant_revision": 1,
                "granted_at": "2026-10-07T00:00:00Z", "granted_by_primary_director_session": director}}},
            "agents": {},
            "actions": {"rotate-1": {"action_id": "rotate-1", "request_sha256": "a".repeat(64),
                "operation": "director_rotate", "project_id": "alpha", "caller_cutex_session": director,
                "phase": "online", "known_successor_cutex_session": successor,
                "created_at": "2026-10-07T00:00:00Z", "updated_at": "2026-10-07T00:00:00Z"}},
            "authority_receipts": {},
            "failure_events": {},
        }))
        .unwrap();
        let roles = |session| roles_from(Some(&snapshot), session);
        assert!(roles(director).director);
        assert!(roles(operator).operator && !roles(operator).director);
        // r219: a legacy in-flight director_rotate still decodes but no
        // longer makes its successor a Director.
        assert_eq!(roles(successor), AgentRoles::default());
        assert_eq!(roles(member), AgentRoles::default());
        assert_eq!(roles_from(None, director), AgentRoles::default());
    }

    #[test]
    fn owner_notify_goes_to_directors_and_owner_created_agents() {
        let owner_created = AgentRoles::default();
        let child = AgentRoles { agent_created: true, ..AgentRoles::default() };
        let director_child = AgentRoles { director: true, agent_created: true, ..AgentRoles::default() };
        let has = |roles, audience| tool_names_for_audience(roles, audience).contains(&"owner_notify");
        assert!(has(owner_created, "ownerFacing"));
        assert!(!has(child, "ownerFacing"));
        assert!(has(director_child, "ownerFacing"));
        assert!(tool_names_for(owner_created).contains(&"owner_notify"));
        assert!(has(child, "everyone"));
        assert!(!has(owner_created, "directors") && has(director_child, "directors"));
        assert!(!has(director_child, "none"));
        // Other tools do not depend on the knob.
        assert_eq!(
            tool_names_for_audience(child, "none").len() + 1,
            tool_names_for_audience(child, "everyone").len()
        );
    }
}
