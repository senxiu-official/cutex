//! Inter-agent registry, message routing, delivery modes.
//!
//! Target boundary: keep volatile `runtime_agent_id` routing here, while durable
//! `cutex_session` identity remains in `session`.

pub mod agent_auth;
pub mod audit;
pub mod boundary;
pub mod client;
pub mod delivery;
pub mod groups;
pub mod identity;
pub mod job_completion;
pub mod launch;
pub mod mcp;
pub mod mcp_browser;
pub mod peer_tell;
pub mod model;
pub mod projection;
pub mod queue;
pub mod routing;
pub mod server;
pub mod service;
pub mod store;
pub mod tool_policy;
pub mod wake_log;
pub mod web_search;

pub mod alarms;
