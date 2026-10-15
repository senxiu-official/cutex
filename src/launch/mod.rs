//! Command construction for cute-codex, Claude-compatible CLIs, Docker, proxy
//! policy, and launch-time environment injection.
//!
//! Target boundary: this module builds commands; durable lifecycle policy lives
//! in `runtime`.

pub mod chatgpt_auth_file;
pub mod args;
pub mod command;
pub mod docker;
pub mod env;
pub mod job_mcp;
pub mod local_deployment;
pub mod native_features;
pub mod profile;
pub mod program;
pub mod runtime;
pub mod scoped_mcp;
pub mod selected_profile;
pub mod selected_status;
pub mod shared_config;
pub mod stock;
pub mod thread_retention;
pub mod version_registry;

pub mod session_display;

pub mod native_history;
pub mod native_plan;
pub mod native_token_usage;

pub mod status_animation;

pub mod agent_status;
