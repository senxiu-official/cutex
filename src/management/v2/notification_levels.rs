//! Root-only notification levels (capability `notification-levels`; cutex
//! `docs/NOTIFICATION-LEVELS.md`):
//!
//! - `GET /v2/notification-levels` → registry (levels, methods, fixed cells,
//!   presets of both tables, events), the owner's stored settings and the
//!   effective tables;
//! - `PATCH /v2/notification-levels` → partial update (same body as
//!   `/v2/settings` `notificationLevels`), answers the new view plus
//!   `changed`;
//! - `POST /v2/notification-levels/evaluate {events, agentId?, projectId?}` →
//!   the decision cutex would make (level, methods, owner_notify channels).
//!
//! Settings live in `config.json` `notification_levels` and apply at once.

use std::net::TcpStream;

use serde_json::{json, Value};

use crate::config::store::{load_codez_config_checked, save_codez_config};
use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::notification_levels::{registry, view, Evaluator, Scope, PATH};

pub const EVALUATE_PATH: &str = "/v2/notification-levels/evaluate";
const MAX_EVALUATE_EVENTS: usize = 32;

pub fn is_path(path: &str) -> bool {
    path == PATH || path == EVALUATE_PATH
}

fn bad_request(stream: &mut TcpStream, code: &str, message: &str) -> anyhow::Result<()> {
    super::server::write_v2_error(stream, 400, "Bad Request", code, message, false, json!({}))
}

fn unavailable(stream: &mut TcpStream, error: anyhow::Error) -> anyhow::Result<()> {
    super::server::write_v2_error(
        stream,
        503,
        "Service Unavailable",
        "settings_unavailable",
        "the cutex config could not be read",
        true,
        json!({ "diagnostic": format!("{error:#}") }),
    )
}

/// The view for a config.
pub fn resource(config: &crate::profiles::model::CodezConfig) -> Value {
    view(&registry(), &config.notification_levels, &config.owner_notify.effective())
}

/// Parse an evaluate body: (events, agentId, projectId).
pub fn parse_evaluate(body: &[u8]) -> Result<(Vec<String>, Option<String>, Option<Option<String>>), String> {
    let value: Value = serde_json::from_slice(body).map_err(|error| format!("JSON object body required: {error}"))?;
    let object = value.as_object().ok_or("JSON object body required")?;
    if let Some(key) = object.keys().find(|key| !["events", "agentId", "projectId"].contains(&key.as_str())) {
        return Err(format!("unsupported field: {key}"));
    }
    let events: Vec<String> = object
        .get("events")
        .and_then(Value::as_array)
        .ok_or("events: a non-empty array of event ids")?
        .iter()
        .map(|event| event.as_str().map(str::to_string).ok_or("events: entries must be strings"))
        .collect::<Result<_, _>>()?;
    if events.is_empty() || events.len() > MAX_EVALUATE_EVENTS {
        return Err(format!("events: 1-{MAX_EVALUATE_EVENTS} event ids"));
    }
    let registry = registry();
    if let Some(unknown) = events.iter().find(|event| registry.event(event).is_none()) {
        return Err(format!("events: unknown event {unknown}"));
    }
    let agent = match object.get("agentId") {
        None | Some(Value::Null) => None,
        Some(Value::String(agent)) if !agent.is_empty() => Some(agent.clone()),
        Some(_) => return Err("agentId: a cutexSessionId or null".into()),
    };
    // Absent = unknown project; null = independent agent.
    let project = match object.get("projectId") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(Value::String(project)) if !project.is_empty() => Some(Some(project.clone())),
        Some(_) => return Err("projectId: a project id or null".into()),
    };
    Ok((events, agent, project))
}

/// The decision for an evaluate request.
pub fn evaluate(evaluator: &Evaluator, events: &[String], agent: Option<&str>, project: Option<Option<&str>>) -> Value {
    let ids: Vec<&str> = events.iter().map(String::as_str).collect();
    let decision = evaluator.decide(&ids, Scope { agent, project });
    json!({
        "level": decision.level,
        "event": decision.event,
        "actions": decision.actions,
        "channels": decision.channels,
        "muted": decision.muted,
        "needsYou": decision.needs_you(),
    })
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    match (request.method.as_str(), path) {
        ("GET", PATH) => match load_codez_config_checked() {
            Ok(config) => write_json_response(stream, 200, "OK", &resource(&config)),
            Err(error) => unavailable(stream, error),
        },
        ("PATCH", PATH) => {
            let value: Value = match serde_json::from_slice(&request.body) {
                Ok(value) => value,
                Err(error) => return bad_request(stream, "invalid_request", &format!("JSON object body required: {error}")),
            };
            // Shape check before the config is read.
            if let Err(message) = crate::config::notification_levels::NotificationLevelSettings::default().patched(
                &value,
                &registry(),
                &Default::default(),
            ) {
                return bad_request(stream, "invalid_request", &message);
            }
            let mut config = match load_codez_config_checked() {
                Ok(config) => config,
                Err(error) => return unavailable(stream, error),
            };
            let next = match config.notification_levels.patched(&value, &registry(), &config.owner_notify.effective()) {
                Ok(next) => next,
                Err(message) => return bad_request(stream, "invalid_request", &message),
            };
            let changed = config.notification_levels != next;
            if changed {
                config.notification_levels = next;
                save_codez_config(&config)?;
            }
            let mut body = resource(&config);
            body["changed"] = json!(changed);
            write_json_response(stream, 200, "OK", &body)
        }
        ("POST", EVALUATE_PATH) => match parse_evaluate(&request.body) {
            Ok((events, agent, project)) => {
                let evaluator = Evaluator::current();
                let project = project.as_ref().map(|project| project.as_deref());
                write_json_response(stream, 200, "OK", &evaluate(&evaluator, &events, agent.as_deref(), project))
            }
            Err(message) => bad_request(stream, "invalid_request", &message),
        },
        _ => super::server::write_v2_error(
            stream,
            405,
            "Method Not Allowed",
            "method_not_allowed",
            "use GET or PATCH for /v2/notification-levels and POST for /v2/notification-levels/evaluate",
            false,
            json!({}),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::notification_levels::NotificationLevelSettings;
    use crate::config::owner_notify::OwnerNotifySettings;

    #[test]
    fn resource_shows_registry_settings_and_default_tables() {
        let config = crate::profiles::model::CodezConfig::default();
        let body = resource(&config);
        assert_eq!(body["contractVersion"], 1);
        assert_eq!(body["registryVersion"], 2);
        assert_eq!(body["settings"]["eventPreset"], "default");
        assert_eq!(body["settings"]["methodPreset"], "default");
        assert_eq!(body["effective"]["eventPreset"], "default");
        assert_eq!(body["effective"]["methodPreset"], "default");
        assert_eq!(body["effective"]["eventLevels"]["ownerNotify.info"], "record");
        assert_eq!(body["effective"]["methods"]["urgent"]["bigAlert"], true);
        assert_eq!(body["effective"]["legacyNeedsYouRequiresChannel"], false);
        let ids: Vec<&str> = body["methodPresets"].as_array().unwrap().iter().map(|p| p["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["quiet", "default", "all"]);
        let ids: Vec<&str> = body["eventPresets"].as_array().unwrap().iter().map(|p| p["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["default"]);
        let events = body["events"].as_array().unwrap();
        let info = events.iter().find(|event| event["id"] == "ownerNotify.info").unwrap();
        assert_eq!(info["levels"], json!({"default": "record"}));
        assert_eq!(info["level"], "record");
        assert_eq!(info["appliedBy"], json!(["cutex"]));
        assert_eq!(body["fixedCells"]["record"]["needsYou"], false);
        // ownerNotify.delivery no longer drives the methods table.
        let mut custom = crate::profiles::model::CodezConfig::default();
        custom.owner_notify.delivery.info = vec![];
        custom.owner_notify.delivery.attention = vec![];
        let body = resource(&custom);
        assert_eq!(body["effective"]["methods"]["inform"]["desktop"], true);
        assert_eq!(body["effective"]["methodPreset"], "default");
        // A stored legacy (older binary) reads as default.
        let mut old = crate::profiles::model::CodezConfig::default();
        old.notification_levels.event_preset = Some("legacy".into());
        old.notification_levels.method_preset = Some("legacy".into());
        let body = resource(&old);
        assert_eq!(body["settings"]["eventPreset"], "default");
        assert_eq!(body["effective"]["methodPreset"], "default");
    }

    fn roundtrip(raw: String) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let mut stream = std::net::TcpStream::connect(address).unwrap();
            stream.write_all(raw.as_bytes()).unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            response
        });
        let (mut stream, _) = listener.accept().unwrap();
        let request = crate::http::server::read_simple_http_request(&mut stream).unwrap();
        handle(&mut stream, &request).unwrap();
        drop(stream);
        client.join().unwrap()
    }

    fn http(method: &str, path: &str, body: &str) -> String {
        roundtrip(format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ))
    }

    #[test]
    fn route_validates_before_touching_the_config_and_evaluates() {
        // Invalid PATCH bodies are refused before the config is read or written.
        for body in [
            r#"{"events":{"preset":"loud"}}"#,
            r#"{"events":{"preset":"legacy"}}"#,
            r#"{"methods":{"preset":"legacy"}}"#,
            r#"{"methods":{"cells":{"inform":{"fax":true}}}}"#,
            "[",
            r#"{"x":1}"#,
        ] {
            let response = http("PATCH", PATH, body);
            assert!(response.starts_with("HTTP/1.1 400"), "{body}: {response}");
            assert!(response.contains("invalid_request"), "{response}");
        }
        let response = http("DELETE", PATH, "");
        assert!(response.starts_with("HTTP/1.1 405"), "{response}");
        let response = http("GET", EVALUATE_PATH, "");
        assert!(response.starts_with("HTTP/1.1 405"), "{response}");
        let response = http("POST", EVALUATE_PATH, r#"{"events":["nope.event"]}"#);
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        // Evaluate reads the current settings (default unless the test home has others).
        let response = http("POST", EVALUATE_PATH, r#"{"events":["native.approval"],"agentId":"cutex.a"}"#);
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let body: Value = serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["event"], "native.approval");
        assert_eq!(body["needsYou"], true);
        assert!(is_path(PATH) && is_path(EVALUATE_PATH) && !is_path("/v2/notification-levels/x"));
    }

    #[test]
    fn evaluate_parses_and_decides() {
        assert!(parse_evaluate(br#"{"events":[]}"#).is_err());
        assert!(parse_evaluate(br#"{"events":["no.such"]}"#).is_err());
        assert!(parse_evaluate(br#"{"events":["ownerNotify.info"],"x":1}"#).is_err());
        let (events, agent, project) =
            parse_evaluate(br#"{"events":["ownerNotify.info","ownerNotify.needsReply"],"agentId":"cutex.a","projectId":null}"#)
                .unwrap();
        assert_eq!(agent.as_deref(), Some("cutex.a"));
        assert_eq!(project, Some(None));
        let owner = OwnerNotifySettings::default();
        let defaults = Evaluator::with_owner(&owner);
        let decided = evaluate(&defaults, &events, agent.as_deref(), None);
        assert_eq!(decided["level"], "needsYou");
        assert_eq!(decided["channels"], json!(["desktop", "phone"]));
        let mut settings = NotificationLevelSettings::default();
        settings = settings
            .patched(&json!({"events": {"preset": "default"}, "methods": {"preset": "quiet"}}), &registry(), &owner)
            .unwrap();
        let recommended = Evaluator::new(registry(), &settings, &owner);
        let decided = evaluate(&recommended, &events, agent.as_deref(), None);
        assert_eq!(decided["level"], "needsYou");
        assert_eq!(decided["event"], "ownerNotify.needsReply");
        assert_eq!(decided["channels"], json!([]));
        assert_eq!(decided["needsYou"], true);
    }
}
