//! Launch-time agent bus environment construction.

use crate::agent_bus::identity::{
    account_agent_name, agent_id_for_launch, cutex_agent_hint, normalize_launch_agent_groups,
};
use crate::config::env::{
    CUTEX_AGENT_BUS_TOKEN_ENV_VAR, CUTEX_AGENT_BUS_URL_ENV_VAR, CUTEX_AGENT_GROUPS_ENV_VAR,
    CUTEX_AGENT_HINT_ENV_VAR, CUTEX_AGENT_HOST_ID_ENV_VAR, CUTEX_AGENT_ID_ENV_VAR,
    CUTEX_AGENT_NAME_ENV_VAR,
};
use crate::profiles::model::{CodezConfig, StoredAccount};

pub fn launch_agent_bus_envs(
    global_config: &CodezConfig,
    account: &StoredAccount,
    agent_groups: &[String],
    bus_url: String,
    host_id: String,
) -> Vec<(String, String)> {
    let agent_name = account_agent_name(account);
    let mut envs = vec![(CUTEX_AGENT_BUS_URL_ENV_VAR.to_string(), bus_url)];
    // An interactive `--agent` launch has no durable session to bind a
    // per-agent credential to, and nothing in the native process reads the
    // token (the shell fallback `cutex agent ...` reads the cutex config).
    // The shared root token is never exported to agents; debug builds can
    // opt back in (`CUTEX_DEBUG_SHARED_AGENT_BUS_TOKEN=1`).
    if crate::config::agent_security::debug_shared_bus_token() {
        if let Some(token) = global_config
            .agent_bus_token
            .as_ref()
            .filter(|token| !token.is_empty())
        {
            envs.push((CUTEX_AGENT_BUS_TOKEN_ENV_VAR.to_string(), token.clone()));
        }
    }
    envs.push((
        CUTEX_AGENT_ID_ENV_VAR.to_string(),
        agent_id_for_launch(account),
    ));
    envs.push((CUTEX_AGENT_NAME_ENV_VAR.to_string(), agent_name));
    envs.push((CUTEX_AGENT_HOST_ID_ENV_VAR.to_string(), host_id));
    let groups = normalize_launch_agent_groups(agent_groups);
    if !groups.is_empty() {
        envs.push((CUTEX_AGENT_GROUPS_ENV_VAR.to_string(), groups.join(",")));
    }
    envs.push((CUTEX_AGENT_HINT_ENV_VAR.to_string(), cutex_agent_hint()));
    envs
}
