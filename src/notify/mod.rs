//! Per-session notification preferences. Delivery is owned by Cutex events.
pub mod session;
pub mod service;

pub mod outbound;

pub mod state;

/// Management v2 error envelope for the `/v2/notifications/*` routes:
/// `{contractVersion: 2, error: {source, code, message, retryable, details}}`.
/// Unknown operations are 404 `route_not_found`; everything else (bad JSON,
/// missing fields, invalid values, unreadable state) is 400 `invalid_request`.
pub(crate) fn write_route_error(
    stream: &mut std::net::TcpStream,
    error: &anyhow::Error,
) -> anyhow::Result<()> {
    let (status, reason, code) = route_error_status(error);
    crate::management::v2::server::write_v2_error(
        stream,
        status,
        reason,
        code,
        &format!("{error:#}"),
        false,
        serde_json::json!({}),
    )
}

pub(crate) const UNSUPPORTED_OPERATION: &str = "unsupported notification operation";

fn route_error_status(error: &anyhow::Error) -> (u16, &'static str, &'static str) {
    if error.to_string() == UNSUPPORTED_OPERATION {
        (404, "Not Found", "route_not_found")
    } else {
        (400, "Bad Request", "invalid_request")
    }
}

#[cfg(test)]
mod route_error_tests {
    use std::io::Read;

    fn respond(error: anyhow::Error) -> (String, serde_json::Value) {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let mut stream = std::net::TcpStream::connect(address).unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            response
        });
        let (mut stream, _) = listener.accept().unwrap();
        super::write_route_error(&mut stream, &error).unwrap();
        drop(stream);
        let response = client.join().unwrap();
        let body = response.split("\r\n\r\n").nth(1).unwrap().to_string();
        (response, serde_json::from_str(&body).unwrap())
    }

    #[test]
    fn notification_errors_use_the_v2_envelope() {
        let (response, body) = respond(anyhow::anyhow!("thread_id is required"));
        assert!(response.starts_with("HTTP/1.1 400 Bad Request\r\n"), "{response}");
        assert_eq!(body["contractVersion"], 2);
        assert_eq!(body["error"]["source"], "cutex");
        assert_eq!(body["error"]["code"], "invalid_request");
        assert_eq!(body["error"]["message"], "thread_id is required");
        assert_eq!(body["error"]["retryable"], false);
        assert!(body.get("error").unwrap().is_object());

        let (response, body) = respond(anyhow::anyhow!(super::UNSUPPORTED_OPERATION));
        assert!(response.starts_with("HTTP/1.1 404 "), "{response}");
        assert_eq!(body["error"]["code"], "route_not_found");
    }
}
