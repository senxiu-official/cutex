//! Browser relay v1. Only the Management process owns this ephemeral authority.
//! Neither provider code nor the MCP adapter may create a second relay instance.
pub mod agent_api;
pub mod approval;
mod audit;
pub mod auth;
pub mod commands;
pub mod history;
pub mod http;
#[cfg(test)]
mod idle_bench;
pub mod images;
pub mod model;
pub mod owner;
mod private_store;
pub mod results;
pub mod retained;
pub mod shot_store;
pub mod shots;
pub mod state;

use state::Relay;
use std::sync::{Arc, OnceLock};

static RELAY: OnceLock<Arc<Relay>> = OnceLock::new();

pub fn relay() -> &'static Arc<Relay> {
    RELAY.get_or_init(|| Arc::new(Relay::new()))
}

/// Called after an in-process token removal has committed, before its reply.
/// Does not initialize a relay in CLI/Agent Bus processes.
pub fn credential_revoked(token_id: &str) {
    if let Some(relay) = RELAY.get() {
        relay.revoke_credential(&format!("client:{token_id}"));
    }
}

/// Runtime hooks run in the Management owner process. Other processes neither
/// initialize a relay nor forward/forge an agent request after activeTurn ends.
pub fn turn_completed(session: &str, thread: &str, generation: u64, turn: &str) {
    if let Some(relay) = RELAY.get() {
        shots::completed(relay, session, thread, generation, turn);
    }
}
