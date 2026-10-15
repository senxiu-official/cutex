//! The agent-facing `owner_notify` tool (contract §1). Served on the Agent
//! Bus delegation route (`/api/asks/v1`): the MCP fence gives the caller,
//! arguments never carry identity, and the derived `action_id` makes a
//! retried call return the first result.
//!
//! The description and result never mention channels, devices, rate limits
//! or downgrades: how the owner is reached is the owner's configuration.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use super::store::{Level, NewNotification, Origin, OwnerNotifyStore};
use crate::notification_levels::Evaluator;
use crate::config::owner_notify::OwnerNotifySettings;

pub const OWNER_NOTIFY: &str = "owner_notify";

const DESCRIPTION: &str = "Send a message to the owner (the person you work for). Use it when the owner should know something now or must decide or answer something only they can: important work they are waiting for is done, you are blocked on them, a risk or a mistake they must know about. Do not use it for routine progress; your conversation already shows that. level is how urgent it is for the owner: info (for their information), attention (they should look soon), urgent (they are needed now; use rarely). With needs_reply=true the owner is asked to answer; their reply arrives later as a message from the owner (a new turn), so end your turn or continue other work and do not wait or poll for it. delegation / job: the delegation or job this message is about.";

pub fn definition() -> Value {
    json!({
        "name": OWNER_NOTIFY,
        "description": DESCRIPTION,
        "inputSchema": {
            "type": "object",
            "properties": {
                "text": {"type": "string", "minLength": 1, "maxLength": super::store::MAX_TEXT_CHARS,
                    "description": "The message, written for the owner: what happened and what you need from them, if anything."},
                "level": {"type": "string", "enum": ["info", "attention", "urgent"], "default": "info",
                    "description": "How urgent this is for the owner."},
                "needs_reply": {"type": "boolean", "default": false,
                    "description": "The owner should answer; the answer arrives later as a message from the owner."},
                "delegation": {"type": "string", "maxLength": super::store::MAX_REF_CHARS,
                    "description": "Optional: the delegation id this message is about."},
                "job": {"type": "string", "maxLength": super::store::MAX_REF_CHARS,
                    "description": "Optional: the job id this message is about."},
                "action_id": {"type": "string", "maxLength": 128,
                    "description": "Idempotency key. Optional: a default is derived from this call."}
            },
            "required": ["text"],
            "additionalProperties": false
        }
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    text: String,
    #[serde(default)]
    level: Option<String>,
    #[serde(default)]
    needs_reply: Option<bool>,
    #[serde(default)]
    delegation: Option<String>,
    #[serde(default)]
    job: Option<String>,
}

fn refusal(code: &str, detail: impl Into<String>) -> Value {
    json!({"ok": false, "status": "no_write", "code": code, "detail": detail.into()})
}

fn reference(value: Option<String>, field: &str) -> Result<Option<String>, Value> {
    let Some(value) = value.map(|value| value.trim().to_string()).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if value.chars().count() > super::store::MAX_REF_CHARS || value.chars().any(char::is_control) {
        return Err(refusal("invalid_arguments", format!("{field}: an id of at most {} characters", super::store::MAX_REF_CHARS)));
    }
    Ok(Some(value))
}

/// Who is calling and what the bus knows about it.
pub struct Caller<'a> {
    pub agent_id: &'a str,
    pub agent_name: Option<String>,
    pub turn_id: Option<String>,
    /// The tool policy offers `owner_notify` to this agent now.
    pub allowed: bool,
    /// The caller's project when known (`Some(None)` = independent agent),
    /// for project mutes (notification levels).
    pub project: Option<Option<String>>,
}

/// Handle one call with the default notification tables.
pub fn handle(
    store: &OwnerNotifyStore,
    settings: &OwnerNotifySettings,
    caller: Caller<'_>,
    args: Value,
    action_id: &str,
    now: DateTime<Utc>,
) -> Value {
    handle_with(store, settings, &Evaluator::with_owner(settings), caller, args, action_id, now)
}

/// Handle one call (pure apart from the store).
pub fn handle_with(
    store: &OwnerNotifyStore,
    settings: &OwnerNotifySettings,
    evaluator: &Evaluator,
    caller: Caller<'_>,
    args: Value,
    action_id: &str,
    now: DateTime<Utc>,
) -> Value {
    let result = (|| -> Result<Value, Value> {
        if !caller.allowed {
            return Err(refusal(
                "not_allowed",
                "owner_notify is not available to you. To reach the owner, tell the agent that created you or your project's Director (agent_tell).",
            ));
        }
        let args: Args = serde_json::from_value(if args.is_null() { json!({}) } else { args })
            .map_err(|error| refusal("invalid_arguments", crate::asks::truncate_chars(&error.to_string(), 300)))?;
        let text = args.text.trim().to_string();
        if text.is_empty() || text.chars().count() > super::store::MAX_TEXT_CHARS {
            return Err(refusal("invalid_arguments", format!("text: 1-{} characters", super::store::MAX_TEXT_CHARS)));
        }
        let level = match args.level.as_deref() {
            None => Level::Info,
            Some(value) => Level::parse(value).ok_or_else(|| refusal("invalid_arguments", "level: info, attention or urgent"))?,
        };
        let new = NewNotification {
            agent_id: caller.agent_id.to_string(),
            agent_name: caller.agent_name.clone(),
            text,
            level,
            needs_reply: args.needs_reply.unwrap_or(false),
            delegation: reference(args.delegation, "delegation")?.map(|id| crate::short_ids::stored_id(&id)),
            job: reference(args.job, "job")?,
            turn_id: caller.turn_id.clone(),
        };
        let digest = crate::approvals::request_digest(&json!({
            "text": new.text, "level": level.as_str(), "needsReply": new.needs_reply,
            "delegation": new.delegation, "job": new.job,
        }));
        store
            .with_state(|state| {
                match state.replay(caller.agent_id, action_id, &digest) {
                    Ok(Some(result)) => return Ok(result),
                    Err(()) => {
                        return Err(refusal("idempotency_conflict", "this action_id was used for a different request; use a new one"))
                    }
                    Ok(None) => {}
                }
                let needs_reply = new.needs_reply;
                let project = caller.project.as_ref().map(|project| project.as_deref());
                let record = state.create_decided(new, Origin::Agent, None, settings, evaluator, project, now);
                let mut result = json!({
                    "ok": true,
                    "notified": true,
                    "notificationId": record.notification_id,
                    "detail": if needs_reply {
                        "The owner was notified. Their reply arrives later as a message from the owner (a new turn); do not wait or poll for it."
                    } else {
                        "The owner was notified."
                    },
                });
                if needs_reply {
                    result["needsReply"] = json!(true);
                }
                state.remember(caller.agent_id, action_id, &digest, &result, now);
                Ok(result)
            })
            .map_err(|error| refusal("persistence_unavailable", crate::asks::truncate_chars(&format!("{error:#}"), 300)))?
    })();
    let mut value = result.unwrap_or_else(|error| error);
    value["action_id"] = json!(action_id);
    value
}

/// The Agent Bus delegation route's entry point.
pub fn handle_from_asks(ctx: &crate::asks::tools::ToolContext<'_>, args: Value, action_id: &str) -> Value {
    let info = ctx.world.directory().ok().and_then(|directory| directory.get(ctx.caller).cloned());
    let agent_name = info.as_ref().and_then(|info| info.name.clone());
    let project = info.map(|info| info.project);
    let turn_id = ctx
        .store
        .peek()
        .ok()
        .flatten()
        .and_then(|state| state.presence.get(ctx.caller).and_then(|presence| presence.turn_id.clone()));
    let (settings, evaluator) = Evaluator::current_with_owner();
    let allowed = crate::agent_bus::tool_policy::owner_notify_allowed(ctx.caller, &settings.tool_audience);
    match OwnerNotifyStore::open_default() {
        Ok(store) => handle_with(
            &store,
            &settings,
            &evaluator,
            Caller { agent_id: ctx.caller, agent_name, turn_id, allowed, project },
            args,
            action_id,
            ctx.now,
        ),
        Err(error) => {
            let mut value = refusal("persistence_unavailable", crate::asks::truncate_chars(&format!("{error:#}"), 300));
            value["action_id"] = json!(action_id);
            value
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (OwnerNotifyStore, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("cutex-owner-notify-tool-{}", uuid::Uuid::new_v4().simple()));
        (OwnerNotifyStore::at(dir.join("owner-notify")), dir)
    }

    fn caller(allowed: bool) -> Caller<'static> {
        Caller { agent_id: "cutex.agent-a", agent_name: Some("Kimi".into()), turn_id: Some("t1".into()), allowed, project: None }
    }

    #[test]
    fn notifies_dedupes_and_never_mentions_delivery() {
        let (store, dir) = store();
        let settings = OwnerNotifySettings::default();
        let now = Utc::now();
        let args = json!({"text": "Deploy needs your OK", "level": "urgent", "needs_reply": true, "job": "job-1"});
        let first = handle(&store, &settings, caller(true), args.clone(), "act-1", now);
        assert_eq!(first["ok"], true);
        assert_eq!(first["notified"], true);
        assert_eq!(first["needsReply"], true);
        assert_eq!(first["action_id"], "act-1");
        let text = first.to_string().to_lowercase();
        for hidden in ["desktop", "phone", "alert", "channel", "rate", "downgrad", "device"] {
            assert!(!text.contains(hidden), "result mentions {hidden}: {text}");
        }
        let description = definition().to_string().to_lowercase();
        for hidden in ["desktop", "phone", "channel", "rate limit", "downgrad", "device"] {
            assert!(!description.contains(hidden), "description mentions {hidden}");
        }
        let again = handle(&store, &settings, caller(true), args, "act-1", now);
        assert_eq!(again["notificationId"], first["notificationId"]);
        assert_eq!(store.snapshot().unwrap().notifications.len(), 1);
        let conflict = handle(&store, &settings, caller(true), json!({"text": "other"}), "act-1", now);
        assert_eq!(conflict["code"], "idempotency_conflict");
        let record = store.snapshot().unwrap().notifications.into_values().next().unwrap();
        assert_eq!(record.job.as_deref(), Some("job-1"));
        assert_eq!(record.turn_id.as_deref(), Some("t1"));
        assert_eq!(record.agent_name.as_deref(), Some("Kimi"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A8: a delegation reference in the agent spelling is stored as the
    /// ask id, and both spellings are one action.
    #[test]
    fn delegation_reference_is_stored_as_the_ask_id() {
        let (store, dir) = store();
        let settings = OwnerNotifySettings::default();
        let now = Utc::now();
        let first = handle(&store, &settings, caller(true), json!({"text": "x", "delegation": "delegation-1a2b3c"}), "a", now);
        let again = handle(&store, &settings, caller(true), json!({"text": "x", "delegation": "ask-1a2b3c"}), "a", now);
        assert_eq!(first, again, "replayed");
        let record = store.snapshot().unwrap().notifications.into_values().next().unwrap();
        assert_eq!(record.delegation.as_deref(), Some("ask-1a2b3c"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rate_limited_results_look_the_same() {
        let (store, dir) = store();
        let mut settings = OwnerNotifySettings::default();
        settings.rate_limit.per_hour = 1;
        let now = Utc::now();
        let a = handle(&store, &settings, caller(true), json!({"text": "one", "level": "urgent"}), "a", now);
        let b = handle(&store, &settings, caller(true), json!({"text": "two", "level": "urgent"}), "b", now);
        let strip = |mut value: Value| {
            value.as_object_mut().unwrap().remove("notificationId");
            value.as_object_mut().unwrap().remove("action_id");
            value
        };
        assert_eq!(strip(a), strip(b));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn refuses_bad_arguments_and_callers_without_the_tool() {
        let (store, dir) = store();
        let settings = OwnerNotifySettings::default();
        let now = Utc::now();
        assert_eq!(handle(&store, &settings, caller(false), json!({"text": "x"}), "a", now)["code"], "not_allowed");
        assert_eq!(handle(&store, &settings, caller(true), json!({"text": "  "}), "b", now)["code"], "invalid_arguments");
        assert_eq!(handle(&store, &settings, caller(true), json!({"text": "x", "level": "loud"}), "c", now)["code"], "invalid_arguments");
        assert_eq!(handle(&store, &settings, caller(true), json!({"text": "x", "to": "owner"}), "d", now)["code"], "invalid_arguments");
        assert!(store.snapshot().unwrap().notifications.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
