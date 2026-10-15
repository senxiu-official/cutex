mod account_store;
mod upgrade_cli;
mod agent;
mod agent_bus_config;
mod agent_bus_runtime;
mod agent_bus_server;
mod agent_cli;
mod agent_context;
mod agent_management;
mod agent_start_message;
mod active_turn;
mod app;
mod app_server_runtime;
mod app_server_state_sync;
mod app_server_user_input;
mod asks_claude;
mod asks_runtime;
mod auth;
mod human;
mod claude_runtime;
mod claude_quota_continue;
mod claude_ingress;
mod claude_structure;
mod claude_repin;
mod claude_account_cli;
mod human_diagnostics;
mod human_project;
mod im_cli;
mod launch;
mod launch_command;
mod launch_output;
mod launch_presenter;
mod launch_process;
mod launch_session;
mod light_standalone;
#[cfg(test)]
mod legacy;
mod instance_cli;
mod management;
mod management_archive;
mod management_context;
mod terminal_qr;
mod management_control_plane;
mod management_focus;
mod management_lifecycle;
mod notify;
mod status_preferences;
mod service_control;
mod thread_titles;
mod side_chats;
mod session_tui_services;
mod profile;
mod profile_usage;
mod profile_settings;
mod profile_settings_presenter;
mod prompt;
mod root_wizard;
mod session;
mod session_archive;
mod session_attach;
mod session_listing;
mod session_management;
mod session_native_workflow;
mod session_history;
mod session_delete;
mod session_fork;
mod session_presenter;
mod session_reconcile;
mod session_runtime;
mod session_settings;
mod session_start_menu;
mod session_tui;
mod session_tui_actions;
mod session_tui_cutex_projects;
mod session_tui_dispatch;
mod session_tui_input;
mod session_tui_layout;
mod session_tui_jobs;
mod session_tui_profile_settings;
mod session_tui_projects;
mod session_tui_recent;
mod session_tui_settings;
mod session_tui_alarms;
mod session_tui_terminal;
mod session_tui_view;
mod session_tui_workspace;
mod session_tui_workspace_events;
mod session_tui_workspace_loading;
mod session_tui_workspace_render;
mod session_wizard;
mod settings;
mod stock_lifecycle;
mod native_management_lifecycle;
mod native_stop;
mod stock_publication;
#[cfg(test)]
mod test_home;
#[cfg(test)]
mod creation_matrix_tests;
mod usage;

pub(crate) use app::run;

pub(crate) fn json_process_error(error: &anyhow::Error) -> Option<serde_json::Value> {
    session_archive::json_process_error(error)
}

mod light_new;

mod session_tui_hosts;

mod remote_sessions;

mod session_tui_remote;

mod session_tui_filters;

mod claude_profile;

mod identity_styles;

#[cfg(all(test, target_os = "linux"))]
mod profiles_umask_tests;
