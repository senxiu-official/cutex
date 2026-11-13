//! The agent-facing `owner_pin` / `owner_pins` tools, served on the Agent Bus
//! delegation route like `owner_notify`: the MCP fence gives the caller,
//! arguments never carry identity, and the action id makes a retried call
//! return the first result. Offered to the same agents as `owner_notify`
//! (`ownerNotify.toolAudience`).

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use super::store::{AgentCaller, AgentRequest, OwnerPinsStore};
use super::*;

pub const OWNER_PIN: &str = "owner_pin";
pub const OWNER_PINS: &str = "owner_pins";

const PIN_DESCRIPTION: &str = "Pin something in the owner's Memos (备忘 in the desktop app), where it stays until they or you resolve it: a decision only they can make (kind decide), something they must do (todo), or a fact they will want later (note). Use it for things the owner would otherwise have to scroll back through the conversation to find; not for progress reports, and not for anything urgent (use owner_notify for that). The owner sees Memos without being notified. With key, a later call with the same key updates your open pin instead of adding one. Set state done or dropped (with pin_id or key, and an optional note) when it no longer applies. You can have at most 20 open pins.";
const LIST_DESCRIPTION: &str = "List the pins you put in the owner's Memos (state open: still open; all: also the ones resolved by you or the owner).";

pub fn is_tool(name: &str) -> bool {
    name == OWNER_PIN || name == OWNER_PINS
}

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": OWNER_PIN,
            "description": PIN_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "enum": ["decide", "todo", "note"], "description": "decide: the owner must decide or answer; todo: the owner must do it; note: keep for later. Required for a new pin."},
                    "title": {"type": "string", "minLength": 1, "maxLength": MAX_TITLE_CHARS, "description": "One line the owner can act on. Required for a new pin."},
                    "detail": {"type": "string", "maxLength": MAX_DETAIL_CHARS, "description": "Optional markdown: context, the options, the exact command."},
                    "links": {"type": "array", "maxItems": MAX_LINKS, "items": {"type": "object", "properties": {
                        "target": {"type": "string", "maxLength": MAX_LINK_TARGET_CHARS, "description": "A cutex:artifact/… link, an absolute file path or an http(s) URL."},
                        "label": {"type": "string", "maxLength": MAX_LINK_LABEL_CHARS}
                    }, "required": ["target"], "additionalProperties": false}},
                    "key": {"type": "string", "maxLength": MAX_KEY_CHARS, "description": "Your handle for this pin: the same key updates your open pin with that key."},
                    "pin_id": {"type": "string", "maxLength": 80, "description": "The pin to update or resolve (from an earlier result or owner_pins)."},
                    "state": {"type": "string", "enum": ["done", "dropped"], "description": "Resolve the pin given by pin_id or key: done, or dropped when it no longer applies."},
                    "note": {"type": "string", "maxLength": MAX_NOTE_CHARS, "description": "With state: one line for the owner on how it was resolved."},
                    "action_id": {"type": "string", "maxLength": 128, "description": "Idempotency key. Optional: a default is derived from this call."}
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": OWNER_PINS,
            "description": LIST_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "state": {"type": "string", "enum": ["open", "all"], "default": "open"},
                    "action_id": {"type": "string", "maxLength": 128, "description": "Idempotency key. Optional."}
                },
                "additionalProperties": false
            }
        }),
    ]
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PinArgs {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    links: Option<Vec<Link>>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    pin_id: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    #[serde(default)]
    state: Option<String>,
}

fn refusal(error: &PinError) -> Value {
    let (code, detail) = error.code();
    json!({"ok": false, "status": "no_write", "code": code, "detail": detail})
}

fn args<T: serde::de::DeserializeOwned>(value: Value) -> PinResult<T> {
    serde_json::from_value(if value.is_null() { json!({}) } else { value })
        .map_err(|error| invalid(crate::asks::truncate_chars(&error.to_string(), 300)))
}

/// Validate `owner_pin` arguments into a store request.
pub fn request(value: Value) -> PinResult<AgentRequest> {
    let a: PinArgs = args(value)?;
    let optional = |v: Option<String>, field: &str, max: usize, multiline: bool| -> PinResult<Option<String>> {
        v.filter(|s| !s.trim().is_empty()).map(|s| text(&s, field, max, multiline)).transpose()
    };
    let kind = a.kind.as_deref().map(|k| Kind::parse(k).ok_or_else(|| invalid("kind: decide, todo or note"))).transpose()?;
    let resolve = match a.state.as_deref() {
        None => None,
        Some("done") => Some(State::Done),
        Some("dropped") => Some(State::Dropped),
        Some(_) => return Err(invalid("state: done or dropped")),
    };
    let pin_id = optional(a.pin_id, "pin_id", 80, false)?;
    if pin_id.as_deref().is_some_and(|id| !id.starts_with("pin-") || !id[4..].chars().all(|c| c.is_ascii_alphanumeric() || c == '-')) {
        return Err(invalid("pin_id: a pin id from an earlier result"));
    }
    let request = AgentRequest {
        pin_id,
        key: optional(a.key, "key", MAX_KEY_CHARS, false)?,
        kind,
        title: optional(a.title, "title", MAX_TITLE_CHARS, false)?,
        detail: optional(a.detail, "detail", MAX_DETAIL_CHARS, true)?,
        links: a.links.map(checked_links).transpose()?,
        resolve,
        note: optional(a.note, "note", MAX_NOTE_CHARS, true)?,
    };
    if request.note.is_some() && request.resolve.is_none() {
        return Err(invalid("note: only with state done or dropped"));
    }
    Ok(request)
}

/// Handle one call (pure apart from the store).
pub fn handle_with(store: &OwnerPinsStore, caller: &AgentCaller, allowed: bool, tool: &str, value: Value, action_id: &str, now: DateTime<Utc>) -> Value {
    let result = (|| -> PinResult<Value> {
        if !allowed {
            return Ok(json!({"ok": false, "status": "no_write", "code": "not_allowed",
                "detail": "the owner's Memos are not available to you. To reach the owner, tell the agent that created you or your project's Director (agent_tell)."}));
        }
        if tool == OWNER_PINS {
            let list: ListArgs = args(value)?;
            let all = match list.state.as_deref() {
                None | Some("open") => false,
                Some("all") => true,
                Some(_) => return Err(invalid("state: open or all")),
            };
            let pins = store.agent_list(&caller.agent_id, all)?;
            let rows: Vec<Value> = pins
                .iter()
                .map(|p| json!({"pinId": p.pin_id, "key": p.key, "kind": p.kind.as_str(), "title": p.title, "state": p.state.as_str(), "resolvedBy": p.resolved_by, "createdAt": p.created_at}))
                .collect();
            return Ok(json!({"ok": true, "pins": rows}));
        }
        let request = request(value.clone())?;
        let digest = crate::approvals::request_digest(&value);
        let (result, changed) = store.agent_write(caller, request, action_id, &digest, now)?;
        if let Some(pin) = changed {
            publish_pin(&pin);
        }
        Ok(result)
    })();
    let mut value = result.unwrap_or_else(|error| refusal(&error));
    value["action_id"] = json!(action_id);
    value
}

/// The Agent Bus delegation route's entry point.
pub fn handle_from_asks(ctx: &crate::asks::tools::ToolContext<'_>, tool: &str, args: Value, action_id: &str) -> Value {
    let agent_name = ctx.world.directory().ok().and_then(|directory| directory.get(ctx.caller).and_then(|info| info.name.clone()));
    let turn_id = ctx
        .store
        .peek()
        .ok()
        .flatten()
        .and_then(|state| state.presence.get(ctx.caller).and_then(|presence| presence.turn_id.clone()));
    let audience = crate::config::owner_notify::current().tool_audience;
    let allowed = crate::agent_bus::tool_policy::owner_notify_allowed(ctx.caller, &audience);
    let caller = AgentCaller { agent_id: ctx.caller.to_string(), agent_name, turn_id };
    match OwnerPinsStore::open_default() {
        Ok(store) => handle_with(&store, &caller, allowed, tool, args, action_id, ctx.now),
        Err(error) => {
            let mut value = refusal(&error);
            value["action_id"] = json!(action_id);
            value
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_are_checked() {
        assert!(request(json!({"kind": "todo", "title": "plug the phone in"})).is_ok());
        assert!(request(json!({"kind": "urgent", "title": "x"})).is_err());
        assert!(request(json!({"title": "x", "agent": "someone-else"})).is_err());
        assert!(request(json!({"kind": "note", "title": "a\u{7}b"})).is_err());
        assert!(request(json!({"kind": "note", "title": "x".repeat(MAX_TITLE_CHARS + 1)})).is_err());
        assert!(request(json!({"kind": "note", "title": "x", "detail": "line 1\nline 2"})).is_ok());
        assert!(request(json!({"kind": "note", "title": "x", "links": [{"target": "relative/path.md"}]})).is_err());
        let absolute_file = std::env::temp_dir().join("owner-pin-link.md");
        assert!(request(json!({"kind": "note", "title": "x", "links": [{"target": "cutex:artifact/af-1@2", "label": "v2"}, {"target": "https://example.com"}, {"target": absolute_file}]})).is_ok());
        assert!(request(json!({"pin_id": "pin-../x", "state": "done"})).is_err());
        assert!(request(json!({"key": "k", "note": "only with state"})).is_err());
        assert_eq!(request(json!({"key": "k", "state": "dropped", "note": "done in chat"})).unwrap().resolve, Some(State::Dropped));
    }

    #[test]
    fn absolute_links_are_platform_independent() {
        for target in ["D:/x/y.md", r"D:\x\y.md", r"\\server\share", r"\\server\share\file.md", "/home/user/file.md", "~/file.md"] {
            assert!(request(json!({"links": [{"target": target}]})).is_ok(), "{target}");
        }
        for target in ["relative/path.md", "D:relative.md", r"\rooted.md", r"\\server", r"\\server\", r"\\\share", "1:/file.md"] {
            assert!(request(json!({"links": [{"target": target}]})).is_err(), "{target}");
        }
    }

    #[test]
    fn calls_go_through_the_store_and_refuse_when_not_offered() {
        let dir = super::test_root("owner-pins-tool-test");
        let store = OwnerPinsStore::open(dir.join("owner-pins")).unwrap();
        let caller = AgentCaller { agent_id: "cutex.a".into(), agent_name: Some("Alden".into()), turn_id: Some("t2".into()) };
        let now = Utc::now();
        let refused = handle_with(&store, &caller, false, OWNER_PIN, json!({"kind": "todo", "title": "x"}), "a0", now);
        assert_eq!(refused["code"], "not_allowed");
        let made = handle_with(&store, &caller, true, OWNER_PIN, json!({"kind": "decide", "title": "4 or auto?", "key": "r281"}), "a1", now);
        assert_eq!((made["ok"].clone(), made["created"].clone(), made["action_id"].clone()), (json!(true), json!(true), json!("a1")));
        let bad = handle_with(&store, &caller, true, OWNER_PIN, json!({"title": "no kind"}), "a2", now);
        assert_eq!(bad["code"], "invalid_arguments");
        let list = handle_with(&store, &caller, true, OWNER_PINS, json!({}), "a3", now);
        assert_eq!(list["pins"][0]["key"], "r281");
        let done = handle_with(&store, &caller, true, OWNER_PIN, json!({"key": "r281", "state": "done"}), "a4", now);
        assert_eq!(done["state"], "done");
        assert_eq!(handle_with(&store, &caller, true, OWNER_PINS, json!({"state": "all"}), "a5", now)["pins"][0]["resolvedBy"], "agent");
        let _ = std::fs::remove_dir_all(dir);
    }
}
