//! Minimal project-owned Agent lifecycle service.
//!
//! The provider owns project authority, Agent ownership metadata, action
//! idempotency, and receipts. Process/thread/Agent Bus work remains behind the
//! production lifecycle adapter so the durable state machine can be tested
//! without creating real Agents.

mod archive;
mod bootstrap_intent;
mod create_abandon;
mod durable_adoption;
mod durable_import;
/// Child ⊆ creator permission check for agent-initiated launches.
pub mod envelope;
mod explicit_launch;
/// Delivery state of create / replace start messages (read-only).
pub mod first_message;
mod human_config;
pub use human_config::*;
mod human_runtime;
pub use human_runtime::*;
#[cfg(target_os = "linux")]
mod migration;
#[cfg(target_os = "linux")]
mod migration_files;
mod model;
#[cfg(target_os = "linux")]
pub use migration::*;
mod projects;
mod provider;
mod runtime_package;
mod stock_runtime;
mod session_delete;
mod sparse;
mod store;

pub use archive::*;
pub use bootstrap_intent::*;
pub use create_abandon::*;
pub use durable_adoption::*;
pub use durable_import::*;
pub use explicit_launch::*;
pub use model::*;
pub use projects::*;
pub use provider::*;
pub use runtime_package::*;
pub use stock_runtime::*;
pub use session_delete::*;
pub use sparse::*;
pub use store::*;

pub const AGENT_MANAGEMENT_CONTRACT: &str = "cutex/agent-management/v1";
pub const AGENT_MANAGEMENT_START_CONTROL_TYPE: &str = "cutex.agent_management.start.v1";
/// A notice to the requester of a create / replace (its start message was
/// not delivered). Only the in-process Management principal sends it.
pub const AGENT_MANAGEMENT_NOTICE_CONTROL_TYPE: &str = "cutex.agent_management.notice.v1";
pub const AGENT_MANAGEMENT_SYSTEM_SENDER: &str = "AgentManagementSystem";
pub const AGENT_MANAGEMENT_MAX_BODY_BYTES: usize = 256 * 1024;

mod runtime_permit;
pub use runtime_permit::RuntimeExecutionPermit;
