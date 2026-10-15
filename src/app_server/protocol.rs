use std::fmt;

use serde_json::json;
use serde_json::Map;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RpcResponseOutcome {
    Result(Value),
    Error(RpcError),
}

#[derive(Debug, Clone, PartialEq)]
pub struct RpcResponse {
    pub id: Value,
    pub outcome: RpcResponseOutcome,
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RpcNotification {
    pub method: String,
    pub params: Option<Value>,
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RpcServerRequest {
    pub id: Value,
    pub method: String,
    pub params: Option<Value>,
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum InboundMessage {
    Response(RpcResponse),
    Notification(RpcNotification),
    ServerRequest(RpcServerRequest),
}

impl InboundMessage {
    pub fn method(&self) -> Option<&str> {
        match self {
            Self::Response(_) => None,
            Self::Notification(notification) => Some(&notification.method),
            Self::ServerRequest(request) => Some(&request.method),
        }
    }

    pub fn raw(&self) -> &Value {
        match self {
            Self::Response(response) => &response.raw,
            Self::Notification(notification) => &notification.raw,
            Self::ServerRequest(request) => &request.raw,
        }
    }

    pub fn correlations(&self) -> CorrelationIds {
        correlations(self.raw())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CorrelationIds {
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub item_id: Option<String>,
    pub client_user_message_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolError {
    message: String,
}

impl ProtocolError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ProtocolError {}

pub fn classify_inbound(raw: Value) -> Result<InboundMessage, ProtocolError> {
    let object = raw
        .as_object()
        .ok_or_else(|| ProtocolError::new("app-server message must be a JSON object"))?;
    if let Some(method) = object.get("method") {
        let method = method
            .as_str()
            .filter(|method| !method.is_empty())
            .ok_or_else(|| ProtocolError::new("app-server method must be a non-empty string"))?
            .to_string();
        let params = object.get("params").cloned();
        return if let Some(id) = object.get("id").filter(|id| !id.is_null()) {
            validate_rpc_id(id)?;
            Ok(InboundMessage::ServerRequest(RpcServerRequest {
                id: id.clone(),
                method,
                params,
                raw,
            }))
        } else {
            Ok(InboundMessage::Notification(RpcNotification {
                method,
                params,
                raw,
            }))
        };
    }

    let id = object
        .get("id")
        .filter(|id| !id.is_null())
        .ok_or_else(|| ProtocolError::new("app-server response omitted id"))?;
    validate_rpc_id(id)?;
    let has_result = object.contains_key("result");
    let has_error = object.contains_key("error");
    if has_result == has_error {
        return Err(ProtocolError::new(
            "app-server response must contain exactly one of result or error",
        ));
    }
    let outcome = if has_result {
        RpcResponseOutcome::Result(object.get("result").cloned().unwrap_or(Value::Null))
    } else {
        RpcResponseOutcome::Error(parse_rpc_error(
            object
                .get("error")
                .expect("error key was checked as present"),
        )?)
    };
    Ok(InboundMessage::Response(RpcResponse {
        id: id.clone(),
        outcome,
        raw,
    }))
}

fn validate_rpc_id(id: &Value) -> Result<(), ProtocolError> {
    if id.is_string() || id.as_i64().is_some() {
        Ok(())
    } else {
        Err(ProtocolError::new(
            "app-server id must be a string or signed 64-bit integer",
        ))
    }
}

fn parse_rpc_error(value: &Value) -> Result<RpcError, ProtocolError> {
    let object = value
        .as_object()
        .ok_or_else(|| ProtocolError::new("app-server error must be a JSON object"))?;
    let code = object
        .get("code")
        .and_then(Value::as_i64)
        .ok_or_else(|| ProtocolError::new("app-server error omitted integer code"))?;
    let message = object
        .get("message")
        .and_then(Value::as_str)
        .ok_or_else(|| ProtocolError::new("app-server error omitted message"))?
        .to_string();
    Ok(RpcError {
        code,
        message,
        data: object.get("data").cloned(),
    })
}

pub fn request_message(id: impl Into<Value>, method: &str, params: Value) -> Value {
    json!({ "id": id.into(), "method": method, "params": params })
}

pub fn notification_message(method: &str, params: Option<Value>) -> Value {
    let mut message = Map::new();
    message.insert("method".to_string(), Value::String(method.to_string()));
    if let Some(params) = params {
        message.insert("params".to_string(), params);
    }
    Value::Object(message)
}

pub fn success_response_message(id: Value, result: Value) -> Value {
    json!({ "id": id, "result": result })
}

pub fn error_response_message(id: Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut error = Map::new();
    error.insert("code".to_string(), Value::from(code));
    error.insert("message".to_string(), Value::String(message.to_string()));
    if let Some(data) = data {
        error.insert("data".to_string(), data);
    }
    json!({ "id": id, "error": error })
}

pub fn correlations(value: &Value) -> CorrelationIds {
    let params = value.get("params").unwrap_or(value);
    CorrelationIds {
        thread_id: first_string(params, &["threadId", "thread_id"]),
        turn_id: first_string(params, &["turnId", "turn_id"])
            .or_else(|| nested_string(params, "turn", "id")),
        item_id: first_string(params, &["itemId", "item_id"])
            .or_else(|| nested_string(params, "item", "id")),
        client_user_message_id: first_string(
            params,
            &["clientUserMessageId", "client_user_message_id"],
        )
        .or_else(|| nested_string(params, "item", "clientId")),
    }
}

fn first_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str).map(str::to_string))
}

fn nested_string(value: &Value, parent: &str, key: &str) -> Option<String> {
    value
        .get(parent)
        .and_then(|value| value.get(key))
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_notification_remains_parseable_and_correlated() {
        let message = classify_inbound(json!({
            "method": "future/native/event",
            "params": {
                "threadId": "thread-1",
                "turn": { "id": "turn-1" },
                "item": { "id": "item-1", "clientId": "client-1" },
                "unrecognized": { "kept": true }
            }
        }))
        .expect("notification should parse");

        assert_eq!(message.method(), Some("future/native/event"));
        assert_eq!(
            message.correlations(),
            CorrelationIds {
                thread_id: Some("thread-1".to_string()),
                turn_id: Some("turn-1".to_string()),
                item_id: Some("item-1".to_string()),
                client_user_message_id: Some("client-1".to_string()),
            }
        );
        assert_eq!(message.raw()["params"]["unrecognized"]["kept"], true);
    }

    #[test]
    fn server_request_preserves_string_id_and_payload() {
        let message = classify_inbound(json!({
            "id": "approval-1",
            "method": "item/commandExecution/requestApproval",
            "params": { "threadId": "thread-1", "itemId": "item-1" }
        }))
        .expect("server request should parse");

        let InboundMessage::ServerRequest(request) = message else {
            panic!("expected server request");
        };
        assert_eq!(request.id, "approval-1");
        assert_eq!(request.params.expect("params")["itemId"], "item-1");
    }

    #[test]
    fn rpc_error_response_is_typed_without_losing_data() {
        let message = classify_inbound(json!({
            "id": 7,
            "error": { "code": -32001, "message": "overloaded", "data": { "retry": true } }
        }))
        .expect("response should parse");

        let InboundMessage::Response(response) = message else {
            panic!("expected response");
        };
        assert_eq!(
            response.outcome,
            RpcResponseOutcome::Error(RpcError {
                code: -32001,
                message: "overloaded".to_string(),
                data: Some(json!({ "retry": true })),
            })
        );
    }

    #[test]
    fn malformed_envelopes_are_rejected() {
        for value in [
            json!([]),
            json!({ "method": 4 }),
            json!({ "id": 1 }),
            json!({ "id": 1, "result": {}, "error": {} }),
            json!({ "id": {}, "result": {} }),
        ] {
            assert!(classify_inbound(value).is_err());
        }
    }
}
