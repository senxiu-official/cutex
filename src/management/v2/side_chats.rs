//! Owner-only ephemeral Side chat transport. This route deliberately bypasses
//! the durable native-request/idempotency/event repositories.
use super::workspace::query_params;
use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::management::server::ManagementRequestContext;
use serde_json::{json, Value};
use std::net::TcpStream;

pub type SideChatHandler = fn(&str, &str, Value) -> Result<Value, SideChatError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideChatError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
    pub details: Value,
}

impl SideChatError {
    pub fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            details: json!({}),
        }
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, "invalid_request", message)
    }
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }
}

fn write_error(stream: &mut TcpStream, error: &SideChatError) -> anyhow::Result<()> {
    let reason = match error.status {
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        429 => "Too Many Requests",
        502 => "Bad Gateway",
        _ => "Internal Server Error",
    };
    write_json_response(
        stream,
        error.status,
        reason,
        &json!({"contractVersion":2,"error":{
            "source":"cutex","code":error.code,"message":error.message,"retryable":false,"details":error.details
        }}),
    )
}

use SideChatError as WorkspaceError;

/// An exact session-scoped route; no caller can name an arbitrary native thread.
pub fn parse_path(path: &str) -> Option<(&str, Option<&str>, Option<&str>)> {
    let parts: Vec<_> = path.strip_prefix("/v2/sessions/")?.split('/').collect();
    if !(2..=4).contains(&parts.len())
        || parts[0].is_empty()
        || parts[1] != "side-chats"
        || parts.iter().any(|part| part.is_empty())
    {
        return None;
    }
    Some((parts[0], parts.get(2).copied(), parts.get(3).copied()))
}

pub fn is_path(path: &str) -> bool {
    parse_path(path).is_some()
}

pub fn handle(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    context: ManagementRequestContext,
) -> anyhow::Result<()> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let Some((session, chat, suffix)) = parse_path(path) else {
        return write_error(
            stream,
            &WorkspaceError::new(404, "side_chat_not_found", "unknown Side route"),
        );
    };
    let action = match (request.method.as_str(), chat, suffix) {
        ("GET", None, None) => "list",
        ("POST", None, None) => "open",
        ("GET", Some(_), None) => "get",
        ("DELETE", Some(_), None) => "close",
        ("POST", Some(_), Some("messages")) => "send",
        ("POST", Some(_), Some("interrupt")) => "interrupt",
        ("POST", Some(_), Some("responses")) => "respond",
        _ => {
            return write_error(
                stream,
                &WorkspaceError::new(405, "method_not_allowed", "unsupported Side operation"),
            )
        }
    };
    let mut params = if request.body.iter().all(u8::is_ascii_whitespace) {
        json!({})
    } else {
        match serde_json::from_slice::<Value>(&request.body) {
            Ok(Value::Object(object)) => Value::Object(object),
            _ => {
                return write_error(
                    stream,
                    &WorkspaceError::bad_request("a JSON object is required"),
                )
            }
        }
    };
    if let Some(chat) = chat {
        params["chatId"] = json!(chat);
    }
    if action == "get" {
        let query = query_params(request);
        for key in ["afterRevision", "waitMs"] {
            if let Some(value) = query.get(key) {
                let Ok(number) = value.parse::<u64>() else {
                    return write_error(
                        stream,
                        &WorkspaceError::bad_request(format!("invalid {key}")),
                    );
                };
                params[key] = json!(number);
            }
        }
    }
    match (context.side_chat)(session, action, params) {
        Ok(value) => write_json_response(stream, 200, "OK", &value),
        Err(error) => write_error(stream, &error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn routes_require_exact_parent_and_chat() {
        assert_eq!(
            parse_path("/v2/sessions/a/side-chats/b/messages"),
            Some(("a", Some("b"), Some("messages")))
        );
        for path in [
            "/v2/side-chats",
            "/v2/sessions//side-chats",
            "/v2/sessions/a/side-chats/",
            "/v2/sessions/a/side-chats/b/messages/extra",
        ] {
            assert!(parse_path(path).is_none());
        }
    }

    #[test]
    fn side_conflict_wire_keeps_the_existing_chat_identity() {
        use std::io::Read;
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        write_error(
            &mut stream,
            &SideChatError::new(409, "side_chat_already_open", "already open")
                .with_details(json!({"chatId":"side-existing","anchorTurnId":"turn-original"})),
        )
        .unwrap();
        drop(stream);
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 409"));
        let body: Value = serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(
            body["error"]["details"],
            json!({"chatId":"side-existing","anchorTurnId":"turn-original"})
        );
    }
}
