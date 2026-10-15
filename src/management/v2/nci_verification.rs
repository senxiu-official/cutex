//! Owner-only configuration for the optional NCI verifier; credentials are write-only.
use crate::http::server::{write_json_response, SimpleHttpRequest};
use serde_json::json;
use std::net::TcpStream;

pub(super) fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    #[cfg(all(target_os = "linux", feature = "nci-qstat"))]
    let result = {
        let path = request.path.split('?').next().unwrap_or_default();
        match request.method.as_str() {
            "GET" if path == "/v2/nci-verification/helper" => Ok(crate::nci_verification::helper()),
            "POST" if path == "/v2/nci-verification/key" => {
                crate::nci_verification::generate_key(&request.body)
            }
            "GET" if path == "/v2/nci-verification" => crate::nci_verification::status(),
            "PUT" if path == "/v2/nci-verification/config" => {
                crate::nci_verification::configure(&request.body)
            }
            "DELETE"
                if path == "/v2/nci-verification/config"
                    && (request.body.is_empty()
                        || serde_json::from_slice::<serde_json::Value>(&request.body).ok()
                            == Some(json!({}))) =>
            {
                crate::nci_verification::clear_config()
            }
            _ => Err("qstat_invalid_request"),
        }
    };
    #[cfg(not(all(target_os = "linux", feature = "nci-qstat")))]
    let result: Result<serde_json::Value, &'static str> = {
        let _ = request;
        Err("nci_verification_unavailable")
    };
    match result {
        Ok(value) => write_json_response(stream, 200, "OK", &value),
        Err("qstat_key_exists") => super::server::write_v2_error(
            stream,
            409,
            "Conflict",
            "qstat_key_exists",
            "A qstat key already exists; send replace: true to generate a new one",
            false,
            json!({}),
        ),
        Err(code) => {
            let invalid = code.starts_with("qstat_invalid_")
                || matches!(
                    code,
                    "qstat_host_key_confirmation_required"
                        | "qstat_private_key_required"
                        | "qstat_private_key_encrypted"
                );
            super::server::write_v2_error(
                stream,
                if invalid { 400 } else { 503 },
                if invalid {
                    "Bad Request"
                } else {
                    "Service Unavailable"
                },
                code,
                "NCI verification operation failed",
                !invalid,
                json!({}),
            )
        }
    }
}
