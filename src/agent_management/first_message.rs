//! Delivery state of a create / replace start message ("first message"),
//! for the creator's `agent_create` / `member_replace` result and
//! `agent_list` (research §8.1, F3 / F5). Read-only: nothing is stored.
//! Since `agent-create-task-delegation` a sparse create / replace task is a
//! delegation (`asks::create_task`); a start message remains only for
//! `task_mode: "message"`, reviewed bootstrap intents and full-spec calls.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::management::v2::agent_bus_state::ManagementStartMessage;

use super::{AgentManagementSnapshot, AgentOperationKind};

/// `queued` (not delivered yet), `delivered`, `quarantined`, `failed` or
/// `discarded` (by the owner). Only a context receipt is `delivered`: a
/// Claude durable-inbox acceptance (`inbox_accepted`) is still `queued`.
pub fn public_state(raw: &str) -> &'static str {
    match raw {
        "delivered" => "delivered",
        "quarantined" => "quarantined",
        "failed" => "failed",
        "discarded" => "discarded",
        _ => "queued",
    }
}

/// Whether `state` (a [`public_state`]) is final.
pub fn is_final(state: &str) -> bool {
    state != "queued"
}

/// What a final, undelivered start message means for its sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Undelivered {
    /// No native submission evidence: the agent never got it.
    NotDelivered,
    /// The agent's runtime may hold it: it may have acted on it.
    Unknown,
    /// The owner discarded it on purpose.
    OwnerDiscarded,
}

impl Undelivered {
    fn of(error: Option<&Value>, native: bool) -> Self {
        let field = |name: &str| error.and_then(|error| error.get(name)).and_then(Value::as_str);
        if field("source") == Some("human")
            && matches!(
                field("code"),
                Some(crate::management::v2::agent_bus_state::OWNER_DISCARD_CODE | "stale_message_discarded")
            )
        {
            Self::OwnerDiscarded
        } else if native {
            Self::Unknown
        } else {
            Self::NotDelivered
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::NotDelivered => "not_delivered",
            Self::Unknown => "unknown",
            Self::OwnerDiscarded => "owner_discarded",
        }
    }
}

/// The JSON report of one start message. A final undelivered one carries
/// `outcome`: `not_delivered` (safe to send again), `unknown` (the agent's
/// runtime may have received it) or `owner_discarded`.
pub fn report(message: &ManagementStartMessage) -> Value {
    let state = public_state(&message.state);
    let mut value = json!({"message_id": message.message_id, "state": state});
    if let Some(error) = &message.error {
        if state != "delivered" {
            value["error"] = json!({
                "code": error.get("code").cloned().unwrap_or(Value::Null),
                "message": error.get("message").cloned().unwrap_or(Value::Null),
            });
            if let Some(details) = error.get("details") {
                value["error"]["details"] = details.clone();
            }
        }
    }
    match state {
        "queued" if message.state == "inbox_accepted" => {
            value["note"] = json!("queued is not delivered: it is in the agent's inbox but not read into its context yet; agent_list shows first_message until it is delivered");
        }
        "queued" => {
            value["note"] = json!("queued is not delivered: the agent has not read it yet; agent_list shows first_message until it is delivered");
        }
        "quarantined" | "failed" | "discarded" => {
            let outcome = Undelivered::of(message.error.as_ref(), message.native_submission_present);
            value["outcome"] = json!(outcome.name());
            value["note"] = json!(match outcome {
                Undelivered::NotDelivered => "not delivered: the agent exists but never got this message; send the brief again with agent_delegate".to_string(),
                Undelivered::Unknown => "delivery outcome unknown: the agent's runtime may already have received this message and acted on it; check what the agent did before giving it the brief again, a second copy may repeat the work".to_string(),
                Undelivered::OwnerDiscarded => format!(
                    "discarded by the owner on purpose{}; do not send the brief again unless the owner asks",
                    if message.native_submission_present { " (the agent's runtime may already have received it)" } else { "" }
                ),
            });
        }
        _ => {}
    }
    value
}

/// The start message `id` (its Agent Bus id, or the action's external
/// message id as an exact replay reports it) to `recipient`.
pub fn find<'a>(
    messages: &'a [ManagementStartMessage],
    id: &str,
    recipient: &str,
) -> Option<&'a ManagementStartMessage> {
    messages.iter().find(|message| {
        message.to_cutex_session_id == recipient
            && (message.message_id == id || message.external_message_id == id)
    })
}

/// Agents (by durable id) whose create / replace start message is not
/// delivered (yet), with its report.
pub fn undelivered_by_agent(
    snapshot: &AgentManagementSnapshot,
    messages: &[ManagementStartMessage],
) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    for action in snapshot.actions.values() {
        if !matches!(action.operation, AgentOperationKind::Create | AgentOperationKind::Replace) {
            continue;
        }
        let (Some(external), Some(successor)) = (
            action.external_message_id.as_deref(),
            action.known_successor_cutex_session.as_ref(),
        ) else {
            continue;
        };
        let Some(message) = messages.iter().find(|message| {
            message.external_message_id == external && message.to_cutex_session_id == successor.as_str()
        }) else {
            continue;
        };
        if public_state(&message.state) != "delivered" {
            out.insert(successor.as_str().to_string(), report(message));
        }
    }
    out
}

/// [`undelivered_by_agent`] on this host's stores; empty when either is
/// unavailable.
pub fn undelivered_on_host() -> BTreeMap<String, Value> {
    let Ok(snapshot) = super::AgentManagementStore::open_default().and_then(|store| {
        store.snapshot().map_err(anyhow::Error::new)
    }) else {
        return BTreeMap::new();
    };
    let Ok(messages) = crate::management::v2::agent_bus_state::agent_bus_message_repository()
        .and_then(|repository| repository.management_start_messages())
    else {
        return BTreeMap::new();
    };
    undelivered_by_agent(&snapshot, &messages)
}

/// A notice to the agent that asked for a create / replace whose start
/// message ended undelivered (F4). Only [`requester_notice`] builds one, and
/// only for a start message the durable store shows quarantined or failed.
pub struct RequesterNotice {
    pub principal: crate::agent_bus::identity::AgentManagementSystemPrincipal,
    pub metadata: super::AgentManagementMessageMetadata,
    /// The Director or operator that asked.
    pub requester: crate::role_revision::CutexSessionId,
    pub text: String,
    /// Stable per start message: a repeated ACK sends no second notice.
    pub external_message_id: String,
}

/// The notice for the acknowledged Agent Bus message `message_id`, when it
/// is an Agent Management start message that ended quarantined or failed.
pub fn requester_notice(message_id: &str) -> anyhow::Result<Option<RequesterNotice>> {
    use anyhow::Context;
    let repository = crate::management::v2::agent_bus_state::agent_bus_message_repository()?;
    let Some(snapshot) = repository.snapshot_by_message_id(message_id)? else {
        return Ok(None);
    };
    if !matches!(public_state(&snapshot.state), "quarantined" | "failed") {
        return Ok(None);
    }
    let canonical = repository.canonical_message(message_id)?;
    if canonical.from != super::AGENT_MANAGEMENT_SYSTEM_SENDER
        || canonical.control_type.as_deref() != Some(super::AGENT_MANAGEMENT_START_CONTROL_TYPE)
    {
        return Ok(None);
    }
    let metadata: super::AgentManagementMessageMetadata = serde_json::from_value(
        canonical.control_payload.clone().context("start message metadata missing")?,
    )?;
    let requester = metadata
        .requested_by_operator
        .clone()
        .unwrap_or_else(|| metadata.requested_by_director.clone());
    let agent = &snapshot.to_cutex_session_id;
    let name = crate::session::store::with_cutex_session_store(|store| {
        store.sessions.values().find(|record| &record.cutex_session_id == agent)
            .and_then(|record| record.formal_agent_name.clone())
    })
    .ok()
    .flatten();
    let label = match name {
        Some(name) => format!("{name} ({})", crate::short_ids::display(agent)),
        None => crate::short_ids::display(agent).to_string(),
    };
    let text = notice_text(&label, agent, &snapshot.state, snapshot.error.as_ref(), snapshot.may_have_reached_runtime());
    Ok(Some(RequesterNotice {
        principal: crate::agent_bus::identity::agent_management_system_principal(),
        metadata,
        requester,
        text,
        external_message_id: format!("mgmt-notice-{message_id}"),
    }))
}

/// The requester notice text for an undelivered start message to `agent`.
fn notice_text(label: &str, agent: &str, state: &str, error: Option<&Value>, native: bool) -> String {
    let (code, reason) = error.map_or((None, None), |error| {
        (error["code"].as_str().map(str::to_string), error["message"].as_str().map(str::to_string))
    });
    let cause = format!(
        "{}{}",
        code.as_deref().unwrap_or(state),
        reason.map(|reason| format!(" ({reason})")).unwrap_or_default(),
    );
    match Undelivered::of(error, native) {
        Undelivered::NotDelivered => format!(
            "The start message (first message) you gave agent {label} was not delivered: {cause}. The agent exists but never read it; send the brief again with agent_delegate (target {agent})."
        ),
        Undelivered::Unknown => format!(
            "The start message (first message) you gave agent {label} ended {state} without a delivery receipt: {cause}. Its runtime may already have received it and acted on it; check what agent {agent} did before giving it the brief again."
        ),
        Undelivered::OwnerDiscarded => format!(
            "The owner discarded the start message (first message) you gave agent {label}: {cause}.{} Do not send the brief again unless the owner asks.",
            if native { " The agent's runtime may already have received it." } else { "" }
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: &str, external: &str, to: &str, state: &str) -> ManagementStartMessage {
        ManagementStartMessage {
            message_id: id.into(),
            external_message_id: external.into(),
            to_cutex_session_id: to.into(),
            state: state.into(),
            error: (state == "quarantined").then(|| json!({"code":"management_start_provenance_invalid","message":"x","retryable":false})),
            native_submission_present: false,
        }
    }

    fn with(mut message: ManagementStartMessage, error: Value, native: bool) -> ManagementStartMessage {
        message.error = Some(error);
        message.native_submission_present = native;
        message
    }

    fn owner_discard(native: bool) -> Value {
        json!({"source":"human","code":"owner_message_discarded","message":"not needed any more","retryable":false,
            "details":{"previousState":"pending","nativeSubmissionPresent":native}})
    }

    /// R11: a Claude durable-inbox acceptance is not a context receipt.
    #[test]
    fn claude_inbox_acceptance_is_not_delivery() {
        assert_eq!(public_state("inbox_accepted"), "queued");
        assert!(!is_final(public_state("inbox_accepted")));
        let accepted = report(&message("m1", "e1", "cutex.a", "inbox_accepted"));
        assert_eq!(accepted["state"], "queued");
        assert!(accepted["note"].as_str().unwrap().contains("inbox"), "{accepted}");
        let mut snapshot = AgentManagementSnapshot::empty();
        let record: super::super::AgentActionRecord = serde_json::from_value(json!({
            "action_id":"a1","request_sha256":"0".repeat(64),"operation":"create","project_id":"project:alpha",
            "caller_cutex_session":"cutex.director","phase":"complete","known_successor_cutex_session":"cutex.a",
            "external_message_id":"e1","created_at":"2026-10-11T00:00:00Z","updated_at":"2026-10-11T00:00:00Z"})).unwrap();
        snapshot.actions.insert(record.action_id.clone(), record);
        let undelivered = undelivered_by_agent(&snapshot, &[message("m1", "e1", "cutex.a", "inbox_accepted")]);
        assert_eq!(undelivered["cutex.a"]["state"], "queued", "agent_list uses the same definition");
    }

    /// R12: confirmed-not-delivered vs outcome unknown vs owner discard.
    #[test]
    fn undelivered_outcomes_are_distinguished() {
        let confirmed = report(&message("m1", "e1", "cutex.a", "quarantined"));
        assert_eq!(confirmed["outcome"], "not_delivered");
        assert!(confirmed["note"].as_str().unwrap().contains("agent_delegate"));

        let evidence = json!({"code":"management_start_reauthorization_failed","message":"director rotated",
            "details":{"nativeSubmissionPresent":true}});
        let unknown = report(&with(message("m2", "e2", "cutex.a", "quarantined"), evidence, true));
        assert_eq!(unknown["outcome"], "unknown");
        let note = unknown["note"].as_str().unwrap();
        assert!(!note.contains("never") && !note.contains("agent_delegate"), "{note}");
        assert_eq!(unknown["error"]["details"]["nativeSubmissionPresent"], true, "evidence kept");

        for (state, native) in [("discarded", false), ("quarantined", true)] {
            let discarded = report(&with(message("m3", "e3", "cutex.a", state), owner_discard(native), native));
            assert_eq!(discarded["state"], state);
            assert_eq!(discarded["outcome"], "owner_discarded");
            let note = discarded["note"].as_str().unwrap();
            assert!(note.contains("owner") && !note.contains("agent_delegate") && !note.contains("never"), "{note}");
        }
        // The pre-native stale-held discard of the r214 owner route.
        let stale = json!({"source":"human","code":"stale_message_discarded","message":"old","retryable":false});
        assert_eq!(report(&with(message("m4", "e4", "cutex.a", "discarded"), stale, false))["outcome"], "owner_discarded");
    }

    #[test]
    fn requester_notice_text_follows_the_outcome() {
        let confirmed = notice_text("w (a)", "cutex.a", "quarantined",
            Some(&json!({"code":"management_start_provenance_invalid","message":"x"})), false);
        assert!(confirmed.contains("never read it") && confirmed.contains("agent_delegate"), "{confirmed}");
        let unknown = notice_text("w (a)", "cutex.a", "quarantined",
            Some(&json!({"code":"management_start_reauthorization_failed","message":"x"})), true);
        assert!(!unknown.contains("never") && !unknown.contains("agent_delegate"), "{unknown}");
        assert!(unknown.contains("may already"), "{unknown}");
        let discarded = notice_text("w (a)", "cutex.a", "quarantined", Some(&owner_discard(true)), true);
        assert!(discarded.contains("owner") && !discarded.contains("agent_delegate") && !discarded.contains("never"), "{discarded}");
    }

    #[test]
    fn states_and_reports() {
        assert_eq!(public_state("pending"), "queued");
        let quarantined = report(&message("m1", "e1", "cutex.a", "quarantined"));
        assert_eq!(quarantined["state"], "quarantined");
        assert_eq!(quarantined["error"]["code"], "management_start_provenance_invalid");
        assert!(quarantined["note"].as_str().unwrap().contains("agent_delegate"));
        let queued = report(&message("m2", "e2", "cutex.a", "pending"));
        assert!(queued["note"].as_str().unwrap().contains("queued is not delivered"));
        assert!(report(&message("m3", "e3", "cutex.a", "delivered")).get("note").is_none());
        let all = [message("m1", "e1", "cutex.a", "pending"), message("m2", "e2", "cutex.b", "pending")];
        assert_eq!(find(&all, "e2", "cutex.b").unwrap().message_id, "m2");
        assert_eq!(find(&all, "m1", "cutex.a").unwrap().message_id, "m1");
        assert!(find(&all, "m1", "cutex.b").is_none());
    }

    #[test]
    fn undelivered_by_agent_lists_only_unfinished_create_and_replace_messages() {
        let mut snapshot = AgentManagementSnapshot::empty();
        for (action, operation, agent) in [("a1", "create", "cutex.a"), ("a2", "replace", "cutex.b"), ("a3", "create", "cutex.c"), ("a4", "restart", "cutex.d")] {
            let record: super::super::AgentActionRecord = serde_json::from_value(json!({
                "action_id":action,"request_sha256":"0".repeat(64),"operation":operation,"project_id":"project:alpha",
                "caller_cutex_session":"cutex.director","phase":"complete","known_successor_cutex_session":agent,
                "external_message_id":format!("ext-{action}"),
                "created_at":"2026-10-11T00:00:00Z","updated_at":"2026-10-11T00:00:00Z"})).unwrap();
            snapshot.actions.insert(record.action_id.clone(), record);
        }
        let messages = [
            message("m1", "ext-a1", "cutex.a", "quarantined"),
            message("m2", "ext-a2", "cutex.b", "pending"),
            message("m3", "ext-a3", "cutex.c", "delivered"),
            message("m4", "ext-a4", "cutex.d", "pending"),
        ];
        let undelivered = undelivered_by_agent(&snapshot, &messages);
        assert_eq!(undelivered.keys().collect::<Vec<_>>(), ["cutex.a", "cutex.b"]);
        assert_eq!(undelivered["cutex.a"]["state"], "quarantined");
        assert_eq!(undelivered["cutex.b"]["state"], "queued");
    }
}
