use cutex::agent_bus::identity::normalize_launch_agent_groups;
use cutex::config::proxy::effective_proxy_config;
use cutex::config::store::load_codez_config;
use cutex::launch::runtime::runtime_description;
use cutex::profiles::inspect::{
    account_model_api_base, account_model_provider, account_proxy_scope_label,
    account_session_scope_label, session_config_label,
};
use cutex::profiles::model::StoredAccount;
use cutex::session::config::effective_session_config;
use cutex::ui::format::proxy_config_label;

use super::launch_output::LaunchOutput;

pub(crate) fn print_launch_summary(
    account: &StoredAccount,
    agent_mode: bool,
    agent_groups: &[String],
    output: LaunchOutput,
) {
    let global_config = load_codez_config();
    let effective_proxy = effective_proxy_config(account, &global_config);
    let proxy_scope = account_proxy_scope_label(account, &global_config);
    let proxy_label = proxy_config_label(effective_proxy);
    let session_scope = account_session_scope_label(account, &global_config);
    let session_label = session_config_label(effective_session_config(account, &global_config));
    let provider = account_model_provider(account).unwrap_or_else(|| "-".to_string());
    let api = account_model_api_base(account).unwrap_or_else(|| "-".to_string());
    let tool_proxy_mode = if effective_proxy.map(|proxy| proxy.enabled).unwrap_or(false) {
        "direct(excluded)"
    } else {
        "inherit-shell"
    };

    let groups_label = if agent_groups.is_empty() {
        "-".to_string()
    } else {
        normalize_launch_agent_groups(agent_groups).join(",")
    };
    output.line(format_args!(
        "Launch: cli={} profile={} runtime=\"{}\" proxy_scope={} proxy=\"{}\" session_scope={} session={} agent={} provider={} api={} tool_proxy={}",
        account.cli_kind,
        account.name,
        runtime_description(&account.runtime),
        proxy_scope,
        proxy_label,
        session_scope,
        session_label,
        if agent_mode {
            format!("collab groups={groups_label}")
        } else {
            "off".to_string()
        },
        provider,
        api,
        tool_proxy_mode
    ));
}
