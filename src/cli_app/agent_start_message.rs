//! Agent creation follow-ups (research agent-create-usability §8):
//! the create preflight's reasoning check (§8.2), the first-message state in
//! `agent_create` / `member_replace` results (F3) and the requester notice
//! when a start message ends undelivered (F4).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cutex::agent_bus::store::AgentBusState;
use cutex::agent_management::first_message;
use cutex::agent_management::{LifecycleFailure, ManagedAgentSpec};
use cutex::profiles::reasoning;
use serde_json::Value;

/// How long a create / replace result waits for its start message to be
/// delivered (or refused) before it reports `queued`, within the MCP
/// adapter's 30 s budget for the whole call (`agent_bus::client`).
const FIRST_MESSAGE_WAIT: Duration = Duration::from_secs(8);
const CALL_BUDGET: Duration = Duration::from_secs(25);
const FIRST_MESSAGE_POLL: Duration = Duration::from_millis(250);

fn invalid_reasoning(detail: String) -> LifecycleFailure {
    LifecycleFailure::definite("invalid_reasoning", detail)
}

/// `spec.reasoning` against the allowed list (catalog, then Claude, then
/// the native list; `default` always).
pub(super) fn check_reasoning(
    spec: &ManagedAgentSpec,
    catalog: Option<Vec<String>>,
    claude: bool,
) -> Result<(), LifecycleFailure> {
    let allowed = reasoning::allowed(catalog, claude);
    let context = format!(
        "for model {:?} on profile {:?}",
        spec.model,
        spec.profile.as_deref().unwrap_or_default()
    );
    reasoning::check(&spec.reasoning, &allowed, &context).map_err(invalid_reasoning)
}

/// Codex create on the installed native: check an explicit reasoning against
/// the selected model's catalog (the launch review of that exact effort),
/// and resolve `default` / empty to the profile's effort so the agent record
/// never stores `default`.
pub(super) fn resolve_local_reasoning(spec: &mut ManagedAgentSpec) -> Result<(), LifecycleFailure> {
    resolve_local_reasoning_with(spec, local_resolution)
}

/// What the installed native's launch review resolves for a spec.
struct LocalResolution {
    /// The selected model's efforts in the profile's catalog, if it lists any.
    catalog: Option<Vec<String>>,
    /// The effective effort (the profile default for `default`).
    reasoning: Option<String>,
    /// The profile default the model does not offer, and the level used.
    fallback: Option<cutex::launch::selected_profile::EffortFallback>,
}

thread_local! {
    /// The effort fallback of the agent_create running on this thread
    /// (preflight → reply; docs/design/028 (b)).
    static EFFORT_FALLBACK: std::cell::RefCell<Option<cutex::launch::selected_profile::EffortFallback>> = const { std::cell::RefCell::new(None) };
}

/// Start of an agent_create / replace on this thread: forget an older fallback.
pub(super) fn clear_effort_fallback() {
    EFFORT_FALLBACK.with(|slot| slot.borrow_mut().take());
}

/// Adds `effort_fallback: {model, requested, used, reason, note}` to a
/// completed create / replace reply whose preflight replaced the profile
/// default effort (the stored receipt is unchanged; capability
/// `agent-create-effort-fallback`).
pub(super) fn add_effort_fallback(response: &mut Value) {
    let Some(fallback) = EFFORT_FALLBACK.with(|slot| slot.borrow_mut().take()) else { return };
    if response["outcome"]["status"] != "complete" {
        return;
    }
    let result = &mut response["outcome"]["receipt"]["result"];
    if !matches!(result["kind"].as_str(), Some("created" | "replaced")) {
        return;
    }
    let mut value = serde_json::to_value(&fallback).unwrap_or(Value::Null);
    value["note"] = Value::String(format!(
        "the profile's default reasoning effort {} is not offered by {}; the agent uses {}",
        fallback.requested, fallback.model, fallback.used
    ));
    result["effort_fallback"] = value;
}

/// The full launch review of `spec` (model and effort included).
fn local_resolution(spec: &ManagedAgentSpec) -> anyhow::Result<LocalResolution> {
    let configuration = cutex::launch::stock::local_bootstrap_configuration(spec)?;
    let catalog = configuration
        .selected_projection
        .as_ref()
        .and_then(|projection| projection.catalog.as_ref())
        .and_then(|catalog| std::fs::read(&catalog.path).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|catalog| reasoning::catalog_efforts(&catalog, &spec.model));
    Ok(LocalResolution { catalog, reasoning: configuration.reasoning, fallback: configuration.effort_fallback })
}

fn resolve_local_reasoning_with(
    spec: &mut ManagedAgentSpec,
    review: impl Fn(&ManagedAgentSpec) -> anyhow::Result<LocalResolution>,
) -> Result<(), LifecycleFailure> {
    let preflight = |error: anyhow::Error| LifecycleFailure::definite("native_bootstrap_preflight_failed", error.to_string());
    if reasoning::is_default(&spec.reasoning) {
        // Resolve the profile default; the review validates it for the model.
        let configuration = review(spec).map_err(preflight)?;
        spec.reasoning = configuration.reasoning.unwrap_or_default();
        if let Some(fallback) = configuration.fallback {
            EFFORT_FALLBACK.with(|slot| *slot.borrow_mut() = Some(fallback));
        }
        return Ok(());
    }
    // An explicit effort: review exactly what was asked, never the profile
    // default it replaces (R10).
    match review(spec) {
        Ok(configuration) => check_reasoning(spec, configuration.catalog, false),
        Err(error) => {
            // Name the allowed efforts when the effort is what failed and the
            // catalog is readable through the profile default.
            let mut probe = spec.clone();
            probe.reasoning = reasoning::DEFAULT.into();
            if let Ok(configuration) = review(&probe) {
                check_reasoning(spec, configuration.catalog, false)?;
            }
            Err(preflight(error))
        }
    }
}

/// F3: adds `first_message: {message_id, state, error?, note?}` to a
/// completed create / replace receipt that carries a start message, after
/// waiting up to [`FIRST_MESSAGE_WAIT`] for it to be delivered or refused.
/// The stored receipt is unchanged; this is only the reply.
pub(super) fn add_first_message(response: &mut Value, call_started: Instant) {
    let wait = FIRST_MESSAGE_WAIT.min(CALL_BUDGET.saturating_sub(call_started.elapsed()));
    add_first_message_with(response, wait, || {
        cutex::management::v2::agent_bus_state::agent_bus_message_repository()
            .and_then(|repository| repository.management_start_messages())
            .ok()
    })
}

fn add_first_message_with(
    response: &mut Value,
    wait: Duration,
    mut messages: impl FnMut() -> Option<Vec<cutex::management::v2::agent_bus_state::ManagementStartMessage>>,
) {
    if response["outcome"]["status"] != "complete" {
        return;
    }
    let result = &response["outcome"]["receipt"]["result"];
    let recipient = match result["kind"].as_str() {
        Some("created") => result["agent"]["cutex_session_id"].as_str(),
        Some("replaced") => result["successor"]["cutex_session_id"].as_str(),
        _ => None,
    };
    let (Some(recipient), Some(id)) = (recipient, result["message_id"].as_str()) else {
        return;
    };
    let (recipient, id) = (recipient.to_string(), id.to_string());
    let deadline = Instant::now() + wait;
    let report = loop {
        let found = messages()
            .and_then(|messages| first_message::find(&messages, &id, &recipient).map(first_message::report));
        let done = found
            .as_ref()
            .is_some_and(|report| report["state"].as_str().is_some_and(first_message::is_final));
        if done || Instant::now() >= deadline {
            break found.unwrap_or_else(|| {
                serde_json::json!({"message_id": id, "state": "queued",
                    "note": "queued is not delivered: the agent has not read it yet; agent_list shows first_message until it is delivered"})
            });
        }
        std::thread::sleep(FIRST_MESSAGE_POLL);
    };
    response["outcome"]["receipt"]["result"]["first_message"] = report;
}

/// F4: [`cutex::agent_bus::server::AgentBusRequestHandlers::management_start_acked`].
pub(crate) fn management_start_acked(state: &Arc<Mutex<AgentBusState>>, ids: &[String]) {
    for id in ids {
        if let Err(error) = notify_requester(state, id) {
            eprintln!("warning: requester notice for start message {id} failed: {error:#}");
        }
    }
}

fn notify_requester(state: &Arc<Mutex<AgentBusState>>, id: &str) -> anyhow::Result<()> {
    let Some(notice) = first_message::requester_notice(id)? else {
        return Ok(());
    };
    let sessions = cutex::session::store::load_cutex_session_store()?;
    let record = |id: &str| sessions.sessions.values().find(|record| record.cutex_session_id == id);
    let Some(requester) = record(notice.requester.as_str()) else {
        return Ok(());
    };
    let Some(target_runtime) = requester.current_runtime_agent_id.clone() else {
        // Offline: the owner's quarantine notice and agent_list still show it.
        return Ok(());
    };
    // Same source binding as the start message (the Director's runtime).
    let director_runtime = record(notice.metadata.requested_by_director.as_str())
        .and_then(|record| record.current_runtime_agent_id.clone());
    let from_session_id = director_runtime
        .as_ref()
        .map(|_| notice.metadata.requested_by_director.as_str().to_string());
    if requester.claude_session.is_some() && from_session_id.is_none() {
        // Claude renders an Agent Management message as one from the
        // Director's session; without it there is no source to show.
        return Ok(());
    }
    super::agent_bus_server::send_agent_management_notice_response(
        state,
        &notice.principal,
        super::agent_bus_server::AgentManagementSystemMessage {
            metadata: &notice.metadata,
            from_agent_id: director_runtime,
            from_session_id,
            target_runtime_agent_id: &target_runtime,
            target_cutex_session_id: &notice.requester,
            exact_message: &notice.text,
            external_message_id: &notice.external_message_id,
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cutex::management::v2::agent_bus_state::ManagementStartMessage;
    use serde_json::json;

    fn created(message_id: Option<&str>) -> Value {
        json!({"outcome":{"status":"complete","receipt":{"result":{"kind":"created",
            "agent":{"cutex_session_id":"cutex.new"},"message_id":message_id}}}})
    }

    fn start(state: &str) -> ManagementStartMessage {
        ManagementStartMessage {
            message_id: "bus-1".into(),
            external_message_id: "ext-1".into(),
            to_cutex_session_id: "cutex.new".into(),
            state: state.into(),
            error: (state == "quarantined")
                .then(|| json!({"code":"management_start_provenance_invalid","message":"absent"})),
            native_submission_present: false,
        }
    }

    /// R10: the profile default is `high`; model `max-only` supports only
    /// `max`, model `deep` supports `high` and `max`. The fake review fails
    /// like `selected_profile::validate_model` when the effective effort is
    /// not in the catalog.
    fn review(spec: &ManagedAgentSpec) -> anyhow::Result<LocalResolution> {
        let efforts: Vec<String> = match spec.model.as_str() {
            "max-only" => vec!["max".into()],
            "deep" => vec!["high".into(), "max".into()],
            other => anyhow::bail!("unknown model {other}"),
        };
        let effective = if reasoning::is_default(&spec.reasoning) { "high".to_string() } else { spec.reasoning.clone() };
        anyhow::ensure!(efforts.contains(&effective), "catalog model/effort missing or ambiguous");
        Ok(LocalResolution { catalog: Some(efforts), reasoning: Some(effective), fallback: None })
    }

    fn spec(model: &str, effort: &str) -> ManagedAgentSpec {
        serde_json::from_value(json!({"name":"worker","cwd":"/work","profile":"p","runtime_backend":"codex",
            "model":model,"reasoning":effort,"permissions":"workspace-write","sandbox_mode":"workspace-write",
            "approval_policy":"never","groups":[]}))
            .expect("spec")
    }

    #[test]
    fn explicit_reasoning_is_checked_before_the_profile_default() {
        let mut explicit = spec("max-only", "max");
        resolve_local_reasoning_with(&mut explicit, review).expect("a legal explicit effort passes");
        assert_eq!(explicit.reasoning, "max");
        // An illegal explicit effort still gets the allowed list when the
        // profile default is valid for the model.
        let error = resolve_local_reasoning_with(&mut spec("deep", "low"), review).unwrap_err();
        assert_eq!(error.code, "invalid_reasoning");
        assert!(error.detail.contains("\"max\""), "{}", error.detail);
        // `default` resolves to the profile effort, which must itself be valid.
        let mut default = spec("deep", "default");
        resolve_local_reasoning_with(&mut default, review).unwrap();
        assert_eq!(default.reasoning, "high");
        let error = resolve_local_reasoning_with(&mut spec("max-only", ""), review).unwrap_err();
        assert_eq!(error.code, "native_bootstrap_preflight_failed");
    }

    /// Design 028 (b): a `default` effort the review replaced (Zooey: profile
    /// `max`, glm-5.3 low/high/max…) is stated in the create reply.
    #[test]
    fn a_default_effort_fallback_is_stated_in_the_reply() {
        clear_effort_fallback();
        let fallback = cutex::launch::selected_profile::EffortFallback {
            model: "glm-5.3".into(), requested: "max".into(), used: "high".into(), reason: "not_offered_by_model".into(),
        };
        let with_fallback = |_: &ManagedAgentSpec| -> anyhow::Result<LocalResolution> {
            Ok(LocalResolution { catalog: Some(vec!["low".into(), "high".into()]), reasoning: Some("high".into()), fallback: Some(fallback.clone()) })
        };
        let mut default = spec("glm-5.3", "default");
        resolve_local_reasoning_with(&mut default, with_fallback).unwrap();
        assert_eq!(default.reasoning, "high");
        let mut reply = created(None);
        add_effort_fallback(&mut reply);
        let stated = &reply["outcome"]["receipt"]["result"]["effort_fallback"];
        assert_eq!((stated["requested"].as_str(), stated["used"].as_str()), (Some("max"), Some("high")));
        assert!(stated["note"].as_str().unwrap().contains("not offered by glm-5.3"));
        // Taken once; an explicit effort never records one.
        let mut again = created(None);
        add_effort_fallback(&mut again);
        assert!(again["outcome"]["receipt"]["result"].get("effort_fallback").is_none());
        let mut explicit = spec("glm-5.3", "high");
        resolve_local_reasoning_with(&mut explicit, with_fallback).unwrap();
        let mut reply = created(None);
        add_effort_fallback(&mut reply);
        assert!(reply["outcome"]["receipt"]["result"].get("effort_fallback").is_none());
    }

    #[test]
    fn first_message_waits_for_a_final_state() {
        let mut calls = 0;
        let mut response = created(Some("ext-1"));
        add_first_message_with(&mut response, Duration::from_secs(5), || {
            calls += 1;
            Some(vec![start(if calls < 3 { "pending" } else { "delivered" })])
        });
        let first = &response["outcome"]["receipt"]["result"]["first_message"];
        assert_eq!(first["state"], "delivered");
        assert_eq!(first["message_id"], "bus-1");
        assert_eq!(calls, 3);
    }

    #[test]
    fn first_message_reports_quarantine_and_queued() {
        let mut response = created(Some("bus-1"));
        add_first_message_with(&mut response, Duration::ZERO, || Some(vec![start("quarantined")]));
        let first = &response["outcome"]["receipt"]["result"]["first_message"];
        assert_eq!(first["state"], "quarantined");
        assert_eq!(first["error"]["code"], "management_start_provenance_invalid");
        let mut response = created(Some("ext-1"));
        add_first_message_with(&mut response, Duration::ZERO, || Some(vec![start("pending")]));
        assert_eq!(response["outcome"]["receipt"]["result"]["first_message"]["state"], "queued");
        let mut response = created(Some("missing"));
        add_first_message_with(&mut response, Duration::ZERO, || Some(Vec::new()));
        assert_eq!(response["outcome"]["receipt"]["result"]["first_message"]["state"], "queued");
    }

    #[test]
    fn no_start_message_no_field() {
        let mut response = created(None);
        add_first_message_with(&mut response, Duration::ZERO, || panic!("no lookup without a message"));
        assert!(response["outcome"]["receipt"]["result"].get("first_message").is_none());
    }
}
