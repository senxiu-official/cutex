//! Version-isolated Codex app-server transport and protocol boundary.
//!
//! This module speaks JSON-RPC and preserves native app-server methods. Runtime
//! launch, management projection, and agent-bus policy live in their existing
//! subsystems.

pub mod activity_bridge;
pub mod bus_bridge;
pub mod client;
pub mod commands;
pub mod external_input;
pub mod external_recovery;
pub mod journal;
pub mod late_adoption;
pub mod live_settings;
pub mod manager;
pub mod participants;
pub mod presentation;
pub mod protocol;
mod delta_coalesce;
pub mod single_writer;
pub mod runtime;

/// One private diagnostic: static phase labels only, never payloads/credentials.
#[cfg(feature = "stock-launch-test-hook")]
pub fn private_restart_phase(phase: &'static str) {
    if std::env::var("CUTEX_PRESENTATION_DIAGNOSTIC_PHASES").as_deref() != Ok("1") {
        return;
    }
    let Ok(home) = std::env::var("CUTEX_TEST_PRIVATE_HOME") else {
        return;
    };
    if std::env::var("HOME").as_deref() != Ok(home.as_str())
        || !std::path::Path::new(&home)
            .join(".cutex-test-private-home")
            .is_file()
    {
        return;
    }
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    eprintln!(
        "PDIAG {ms} {} {:?} {phase}",
        std::process::id(),
        std::thread::current().id()
    );
}
