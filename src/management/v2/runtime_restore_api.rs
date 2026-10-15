//! `GET|DELETE /v2/runtimes/restore-candidates` (capability
//! `runtime-restore-candidates`; root credential incl. client / device
//! tokens): agents that were online before an unclean stop (host reboot,
//! crash). Read and dismiss only; restoring is the ordinary
//! `cutex/runtime/online` per agent. Store: `session::runtime_intent`.

use std::net::TcpStream;

use serde_json::json;

use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::session::runtime_intent;

pub use runtime_intent::is_path;

pub const EXPORTED_SCHEMA: super::schema_export::ExportedSchema = super::schema_export::ExportedSchema {
    name: "runtime-restore",
    title: "Runtime restore candidates after an unclean stop",
    routes: &["GET /v2/runtimes/restore-candidates", "DELETE /v2/runtimes/restore-candidates"],
    text: include_str!("schema/export/runtime-restore.json"),
};

fn unavailable(stream: &mut TcpStream, error: anyhow::Error) -> anyhow::Result<()> {
    super::server::write_v2_error(stream, 503, "Service Unavailable", "runtime_intent_unavailable", &format!("{error:#}"), true, json!({}))
}

/// Socket entry point; the caller has already verified the root credential.
pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let loaded = runtime_intent::store_path().and_then(|path| Ok((path, crate::session::store::load_cutex_session_store()?)));
    let (path, sessions) = match loaded {
        Ok(loaded) => loaded,
        Err(error) => return unavailable(stream, error),
    };
    match request.method.as_str() {
        "GET" => match runtime_intent::candidates_resource_at(&path, &sessions, &runtime_intent::record_online, &crate::platform::host::current_host_name()) {
            Ok(body) => write_json_response(stream, 200, "OK", &body),
            Err(error) => unavailable(stream, error),
        },
        "DELETE" => match runtime_intent::dismiss_at(&path, &sessions, &runtime_intent::record_online) {
            Ok(dismissed) => write_json_response(stream, 200, "OK", &json!({ "dismissed": dismissed })),
            Err(error) => unavailable(stream, error),
        },
        _ => super::server::write_v2_error(stream, 405, "Method Not Allowed", "method_not_allowed", "only GET and DELETE are allowed on /v2/runtimes/restore-candidates", false, json!({})),
    }
}
