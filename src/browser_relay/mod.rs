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
pub mod script;
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
        let ended_at = std::time::Instant::now();
        let shots = shots::completed(relay, session, thread, generation, turn);
        script::turn_completed(session);
        release_after_turn(relay, session, ended_at, shots);
    }
}

/// Owner decision (design 059): a turn's end gives control back; pages stay.
/// Waits for the turn-end screenshots (they need the lease), bounded.
fn release_after_turn(relay: &Arc<Relay>, session: &str, ended_at: std::time::Instant, shots: Arc<()>) {
    let holds = relay.state.lock().is_ok_and(|state| state.leases.values().any(|lease| lease.view.cutex_session_id == session));
    if !holds {
        return;
    }
    let (relay, session) = (Arc::clone(relay), session.to_owned());
    let release = move || {
        let limit = ended_at + commands::COMMAND_TIMEOUT + std::time::Duration::from_secs(2);
        while Arc::strong_count(&shots) > 1 && std::time::Instant::now() < limit {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if let Ok(mut state) = relay.state.lock() {
            if state::release_after_turn(&mut state, &session, ended_at) > 0 {
                relay.changed.notify_all();
            }
        }
    };
    // No thread: the lease's own idle expiry still ends it.
    let _ = std::thread::Builder::new().name("browser-turn-release".into()).spawn(release);
}
