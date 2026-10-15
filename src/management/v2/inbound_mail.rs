//! Owner-only mailbox configuration. Never return credential or server error text.
use crate::http::server::{write_json_response, SimpleHttpRequest};
use serde_json::json;
use std::net::TcpStream;

pub(super) fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    let result = match request.method.as_str() {
        "GET" => crate::inbound_mail::status(),
        "PUT" => crate::inbound_mail::configure(&request.body),
        "POST" => crate::inbound_mail::test_connection(&request.body),
        "DELETE"
            if request.body.is_empty()
                || serde_json::from_slice::<serde_json::Value>(&request.body).ok()
                    == Some(json!({})) =>
        {
            crate::inbound_mail::clear_config()
        }
        _ => Err("invalid_mail_request"),
    };
    #[cfg(not(target_os = "linux"))]
    let result: Result<serde_json::Value, &'static str> = {
        let _ = request;
        Err("inbound_mail_unsupported")
    };
    match result {
        Ok(mut value) => {
            // Whether the running Job Service reads the notification-address file
            // (`serve --external-notify-file`); cached, no socket IO on this path.
            if let Some(object) = value.as_object_mut() {
                if object.contains_key("nciMailToStatus") {
                    let jobs = super::human_jobs::cached_capabilities();
                    object.insert("jobServiceNotifyFile".into(), json!(jobs.notify_file));
                    object.insert("jobServiceNciSource".into(), json!(jobs.nci));
                }
            }
            write_json_response(stream, 200, "OK", &value)
        }
        Err(code) => {
            let invalid = matches!(
                code,
                "invalid_mail_config"
                    | "invalid_mail_request"
                    | "mail_password_required"
                    | "mail_not_configured"
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
                "Mailbox operation failed",
                !invalid,
                json!({}),
            )
        }
    }
}
