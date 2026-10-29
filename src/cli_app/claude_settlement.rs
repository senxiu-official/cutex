//! Read-only, fail-closed assignment/terminal observations. This never proves
//! consumption, successful delivery, or absence of effects (including no output).
use anyhow::{ensure, Context};
use chrono::Utc;
use cutex::app_server::external_input::{
    Delivery, DeliveryState, Envelope, ProcessingState, Response, SourceKind,
};
use cutex::management::v2::agent_bus_settlement_model::{Evidence, Observation};
use cutex::management::v2::agent_bus_state::AgentBusMessageSnapshot;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};

const MAX_PAGES: usize = 16;
const OBSERVATION_BUDGET: Duration = Duration::from_secs(15);

#[path = "claude_settlement_background.rs"]
mod background;
pub(super) use background::BackgroundObservations;

/// Background work follows the same static advertisement as /v2/capabilities.
/// Explicit owner collect keeps its independent full-proof path below.
pub(super) fn background_enabled() -> bool {
    cutex::management::v2::capabilities::MANAGEMENT_FEATURES
        .iter()
        .any(|(feature, _)| *feature == cutex::management::v2::agent_bus_settlement_model::FEATURE)
}

/// Trusted, understood observations that do not establish close eligibility.
/// Kept separate from transport/unavailable/malformed/unsupported errors.
#[derive(Debug)]
struct Ineligible(&'static str);
impl std::fmt::Display for Ineligible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Ineligible {}

macro_rules! eligible_ensure {
    ($condition:expr, $reason:literal $(,)?) => {
        if !$condition {
            return Err(Ineligible($reason).into());
        }
    };
}

fn classify(result: anyhow::Result<Observation>) -> anyhow::Result<Observation> {
    match result {
        Err(error) if error.downcast_ref::<Ineligible>().is_some() => Ok(Observation::Ineligible),
        result => result,
    }
}

fn remaining(deadline: Instant) -> anyhow::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .context("settlement observation deadline exhausted; outcome unavailable")
}

fn existing_handle(
    manager: &cutex::app_server::manager::AppServerRuntimeManager,
    e: &Envelope,
) -> anyhow::Result<cutex::app_server::client::AppServerHandle> {
    use cutex::app_server::manager::AppServerExactRuntimeHandleError;
    match manager.handle_for_generation(&e.owner_id, e.runtime_generation) {
        Err(AppServerExactRuntimeHandleError::StaleGeneration {
            actual: Some(_), ..
        }) => Err(Ineligible("Claude runtime generation changed").into()),
        result => result.map_err(Into::into),
    }
}

pub(super) fn collect(snapshot: &AgentBusMessageSnapshot) -> anyhow::Result<Observation> {
    classify(collect_connected(
        snapshot,
        Instant::now() + OBSERVATION_BUDGET,
    ))
}

pub(super) fn collect_background(
    snapshot: &AgentBusMessageSnapshot,
    deadline: Instant,
) -> anyhow::Result<Observation> {
    classify(collect_connected(snapshot, deadline))
}

/// Commit-time check called under the message repository write lock. Never
/// read that repository here (including through collect/fence), issue native
/// RPCs, initialize a manager, reconnect, or start a runtime.
pub(super) fn runtime_fence(evidence: &Evidence) -> anyhow::Result<bool> {
    use cutex::app_server::manager::AppServerExactRuntimeHandleError;
    if evidence.runtime != "claude" {
        return Ok(false);
    }
    let manager = super::MANAGER
        .get()
        .context("Claude settlement observer unavailable")?;
    let sessions = cutex::session::store::load_cutex_session_store_shared()?;
    let Some(record) = sessions.sessions.get(&evidence.recipient_id) else {
        return Ok(false);
    };
    if record.cutex_session_id != evidence.recipient_id
        || record.runtime_backend
            != cutex::session::model::CutexSessionRuntimeBackend::ClaudeSubscription
        || record.is_retired()
        || record.current_runtime_agent_id.is_none()
        || record.runtime_generation != evidence.observer_generation()
        || record.app_server_thread_id() != Some(evidence.thread_id.as_str())
    {
        return Ok(false);
    }
    let status = manager
        .status(&evidence.recipient_id)?
        .context("Claude running handle unavailable")?;
    if status.cutex_session_id != evidence.recipient_id
        || status.thread_id != evidence.thread_id
        || status.runtime_generation != evidence.observer_generation()
        || !status.connected
    {
        return Ok(false);
    }
    match manager.handle_for_generation(&evidence.recipient_id, evidence.observer_generation()) {
        Ok(_) => Ok(true),
        Err(AppServerExactRuntimeHandleError::StaleGeneration {
            actual: Some(_), ..
        }) => Ok(false),
        // A missing handle or an unreadable/disappearing connection cannot be
        // interpreted as a successful negative observation of its identity.
        Err(error) => Err(error.into()),
    }
}

fn collect_connected(
    snapshot: &AgentBusMessageSnapshot,
    deadline: Instant,
) -> anyhow::Result<Observation> {
    remaining(deadline)?;
    let Some(envelope) = snapshot.external_input.as_ref() else {
        return Ok(Observation::Ineligible);
    };
    if !eligible(snapshot, envelope) {
        return Ok(Observation::Ineligible);
    }
    // Deliberately do not call manager(), observe(), ensure(), or capability
    // recovery: only a manager/connection already installed by the observer.
    let manager = super::MANAGER
        .get()
        .context("Claude settlement observer unavailable")?;
    // The immutable envelope identifies the historical submission. Only this
    // temporary read binding uses the currently connected observer generation.
    let observing_generation = manager
        .status(&envelope.owner_id)?
        .context("Claude connection missing")?
        .runtime_generation;
    let mut query_envelope = envelope.clone();
    query_envelope.runtime_generation = observing_generation;
    let fence = || -> anyhow::Result<()> {
        let store = cutex::session::store::load_cutex_session_store_shared()?;
        let record = store
            .sessions
            .get(&envelope.owner_id)
            .context("Claude recipient missing")?;
        eligible_ensure!(
            record.runtime_backend
                == cutex::session::model::CutexSessionRuntimeBackend::ClaudeSubscription
                && record.runtime_generation == observing_generation
                && record.app_server_thread_id() == Some(envelope.thread_id.as_str()),
            "Claude runtime binding changed"
        );
        ensure!(
            record.current_runtime_agent_id.is_some(),
            "Claude runtime offline"
        );
        let status = manager
            .status(&envelope.owner_id)?
            .context("Claude connection missing")?;
        eligible_ensure!(
            status.cutex_session_id == envelope.owner_id
                && status.thread_id == envelope.thread_id
                && status.runtime_generation == observing_generation,
            "Claude connection binding changed"
        );
        ensure!(status.connected, "Claude connection unavailable");
        existing_handle(manager, &query_envelope)?;
        let current = cutex::management::v2::agent_bus_state::agent_bus_message_repository()?
            .snapshot_by_message_id(&snapshot.message_id)?
            .ok_or(Ineligible("message disappeared"))?;
        eligible_ensure!(
            eligible(&current, envelope) && current.external_input.as_ref() == Some(envelope),
            "message identity or receipt changed"
        );
        remaining(deadline)?;
        Ok(())
    };
    fence()?;
    let handle = existing_handle(manager, &query_envelope)?;
    collect_read_binding_with_deadline(
        envelope,
        observing_generation,
        |method, params, timeout| Ok(handle.request_with_timeout(method, params, timeout)?),
        fence,
        deadline,
    )
}

fn collect_read_binding(
    frozen: &Envelope,
    observing_generation: u64,
    request: impl FnMut(&str, Value) -> anyhow::Result<Value>,
    fence: impl FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<Observation> {
    let mut request = request;
    collect_read_binding_with_deadline(
        frozen,
        observing_generation,
        |method, params, _timeout| request(method, params),
        fence,
        Instant::now() + OBSERVATION_BUDGET,
    )
}

fn collect_read_binding_with_deadline(
    frozen: &Envelope,
    observing_generation: u64,
    mut request: impl FnMut(&str, Value, Duration) -> anyhow::Result<Value>,
    fence: impl FnMut() -> anyhow::Result<()>,
    deadline: Instant,
) -> anyhow::Result<Observation> {
    let mut query = frozen.clone();
    query.runtime_generation = observing_generation;
    let mut observation = collect_with_deadline(
        &query,
        |method, params| {
            request(
                method,
                params,
                remaining(deadline)?.min(Duration::from_secs(10)),
            )
        },
        fence,
        deadline,
    )?;
    if let Observation::Terminal(evidence) = &mut observation {
        evidence.runtime_generation = frozen.runtime_generation;
        evidence.observing_generation = Some(observing_generation);
    }
    Ok(observation)
}

fn eligible(snapshot: &AgentBusMessageSnapshot, envelope: &Envelope) -> bool {
    snapshot.message_id == envelope.message.id
        && snapshot.to_cutex_session_id == envelope.owner_id
        && matches!(snapshot.state.as_str(), "pending" | "inbox_accepted")
        && snapshot.a4_receipt.is_none()
        && snapshot.external_input_receipt.is_none()
        && envelope.message.event_type == "agent_message"
        && envelope.message.source.kind == SourceKind::Agent
        && envelope.message.delivery != Delivery::Passive
}

// Python store.dump: sorted keys, compact separators, literal Unicode. Native
// views reject floats, so serde_json's integer/string encoding is identical.
fn hash(value: &Value) -> anyhow::Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}

fn local_digest(envelope: &Envelope) -> anyhow::Result<String> {
    let mut metadata = json!({"nativeExternalInput": true});
    if let Some(view) = &envelope.view {
        if view.schema == "cutex.agent-message.v1" {
            if let Some(name) = view.data["senderName"].as_str().filter(|s| {
                s.chars()
                    .any(|c| !(c.is_whitespace() || ('\u{001c}'..='\u{001f}').contains(&c)))
            }) {
                metadata["sourceName"] = json!(name.chars().take(256).collect::<String>());
            }
        }
    }
    if envelope.message.delivery == Delivery::Soon {
        metadata["deliveryMode"] = json!("soon");
    }
    let mut payload = json!({"source":envelope.message.source, "eventType":envelope.message.event_type,
        "text":envelope.message.text, "metadata":metadata});
    if let Some(view) = &envelope.view {
        payload["view"] = serde_json::to_value(view)?;
    }
    hash(&payload)
}

fn attempt_id(thread: &str, turn: &str) -> String {
    // UUIDv5 NAMESPACE_URL, exactly Python uuid.uuid5. SHA1 is only the
    // adapter's bookkeeping algorithm, never an authentication/content digest.
    let mut sha = ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY);
    sha.update(uuid::Uuid::NAMESPACE_URL.as_bytes());
    sha.update(format!("cute-claude:external:{thread}:{turn}").as_bytes());
    let digest = sha.finish();
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest.as_ref()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

fn native(
    request: &mut impl FnMut(&str, Value) -> anyhow::Result<Value>,
    e: &Envelope,
) -> anyhow::Result<Value> {
    let raw = request(
        "thread/externalInput/status",
        json!({"version":1,"ownerId":e.owner_id,
        "threadId":e.thread_id,"runtimeGeneration":e.runtime_generation,"messages":[e.key()]}),
    )?;
    let response: Response = serde_json::from_value(raw.clone())?;
    ensure!(
        response.version == 1 && response.statuses.len() == 1,
        "unsupported/malformed native status response"
    );
    eligible_ensure!(
        response.owner_id == e.owner_id
            && response.thread_id == e.thread_id
            && response.runtime_generation == e.runtime_generation,
        "native runtime binding changed"
    );
    eligible_ensure!(
        response.statuses[0].message_id == e.message.id
            && response.statuses[0].semantic_sha256 == e.semantic_sha256,
        "native message identity/digest conflict"
    );
    if let Some(receipt) = &response.statuses[0].receipt {
        eligible_ensure!(
            receipt.owner_id == e.owner_id
                && receipt.thread_id == e.thread_id
                && receipt.message_id == e.message.id
                && receipt.response_item_id == e.message.id
                && receipt.semantic_sha256 == e.semantic_sha256,
            "native receipt evidence conflict"
        );
    }
    response.validate(
        &cutex::launch::stock::ExternalInputBinding {
            version: 1,
            owner_id: e.owner_id.clone(),
            thread_id: e.thread_id.clone(),
            runtime_generation: e.runtime_generation,
            canonical_byte_limit: Default::default(),
        },
        &[e.key()],
    )?;
    let status = &response.statuses[0];
    eligible_ensure!(
        status.delivery_state == DeliveryState::Pending
            && status.receipt.is_none()
            && matches!(
                status.processing.state,
                ProcessingState::Held | ProcessingState::OutputObserved
            )
            && status.processing.attempt_id.is_some(),
        "no eligible native attempt"
    );
    Ok(raw)
}

fn local(
    request: &mut impl FnMut(&str, Value) -> anyhow::Result<Value>,
    e: &Envelope,
    digest: &str,
) -> anyhow::Result<Value> {
    let raw = request(
        "claude/input/status",
        json!({"threadId":e.thread_id,"messageId":e.message.id}),
    )?;
    ensure!(
        [
            "threadId",
            "messageId",
            "semanticSha256",
            "receiptScope",
            "state"
        ]
        .iter()
        .all(|key| raw[*key].is_string())
            && raw["durable"].is_boolean()
            && raw
                .get("contextPersisted")
                .is_none_or(|v| v.is_null() || v.is_boolean()),
        "malformed local inbox status"
    );
    ensure!(
        ["turnId", "contextTurnId"]
            .iter()
            .all(|key| raw.get(*key).is_none_or(|v| v.is_null() || v.is_string())),
        "malformed actual turn identity"
    );
    eligible_ensure!(
        raw["threadId"] == e.thread_id
            && raw["messageId"] == e.message.id
            && raw["semanticSha256"] == digest
            && raw["durable"] == true
            && raw["receiptScope"] == "local-inbox"
            && raw["contextPersisted"] != true
            && matches!(
                raw["state"].as_str(),
                Some("completed" | "failed" | "interrupted")
            ),
        "ineligible local inbox state/identity"
    );
    Ok(raw)
}

fn actual<'a>(e: &Envelope, local: &'a Value) -> anyhow::Result<&'a str> {
    let context = local["contextTurnId"].as_str().filter(|s| !s.is_empty());
    // Untouched Soon inputs may subsequently run through ordinary FIFO.
    // This selects a candidate only: collect_validated still requires the exact
    // native attempt, terminal history, and matching external_event payload.
    (if e.message.delivery == Delivery::Soon {
        context.or_else(|| local["turnId"].as_str())
    } else {
        local["turnId"].as_str()
    })
    .filter(|s| !s.is_empty())
    .ok_or_else(|| Ineligible("actual Claude turn candidate missing").into())
}

fn pages(
    request: &mut impl FnMut(&str, Value) -> anyhow::Result<Value>,
    method: &str,
    mut params: Value,
    mut visit: impl FnMut(&Value) -> anyhow::Result<bool>,
) -> anyhow::Result<()> {
    let mut cursors = std::collections::HashSet::new();
    for _ in 0..MAX_PAGES {
        let page = request(method, params.clone())?;
        let rows = page["data"].as_array().context("invalid history page")?;
        let mut found = false;
        for row in rows {
            found |= visit(row)?;
        }
        if found {
            return Ok(());
        }
        match page.get("nextCursor") {
            Some(Value::Null) => return Ok(()),
            Some(Value::String(cursor)) if !cursor.is_empty() => {
                ensure!(cursors.insert(cursor.clone()), "history cursor cycle");
                params["cursor"] = json!(cursor);
            }
            _ => anyhow::bail!("invalid history cursor"),
        }
    }
    anyhow::bail!("settlement history read budget exhausted; outcome unavailable")
}

fn external_item(item: &Value, e: &Envelope) -> anyhow::Result<bool> {
    ensure!(
        item["id"].is_string() && item["type"].is_string(),
        "malformed history item"
    );
    if item["id"] != e.message.id {
        return Ok(false);
    }
    eligible_ensure!(
        item["type"] == "functionCallOutput" && item["name"] == "external_event",
        "conflicting external item identity"
    );
    // Also bind the actual projected input, rather than trusting an ID alone.
    let output: Value = serde_json::from_str(
        item["output"]
            .as_str()
            .context("external item output missing")?,
    )?;
    eligible_ensure!(
        output
            == json!({"source":e.message.source,"type":e.message.event_type,"text":e.message.text})
            && item
                .get("externalInputView")
                .cloned()
                .unwrap_or(Value::Null)
                == serde_json::to_value(&e.view)?,
        "external item payload conflict"
    );
    Ok(true)
}

fn history(
    request: &mut impl FnMut(&str, Value) -> anyhow::Result<Value>,
    e: &Envelope,
    actual: &str,
) -> anyhow::Result<Value> {
    let read = request(
        "thread/read",
        json!({"threadId":e.thread_id,"includeTurns":false}),
    )?;
    let thread = &read["thread"];
    ensure!(thread["id"].is_string(), "malformed thread identity");
    eligible_ensure!(
        thread["id"] == e.thread_id,
        "history thread identity changed"
    );
    ensure!(
        thread["modelProvider"] == "cute-claude" && thread["cliVersion"] == "0.8.5",
        "unsupported history provider/version"
    );
    let mut turn = None;
    // Never materialize the bounded full transcript for each stuck message.
    // Turn pages carry metadata only; item pages are scoped to the actual turn.
    pages(
        request,
        "thread/turns/list",
        json!({"threadId":e.thread_id,"itemsView":"notLoaded","limit":200,"sortDirection":"desc"}),
        |candidate| {
            ensure!(
                candidate["id"].is_string() && candidate["status"].is_string(),
                "malformed history turn page"
            );
            if candidate["id"] == actual {
                eligible_ensure!(turn.is_none(), "duplicate actual turn");
                turn = Some(candidate.clone());
                return Ok(true);
            }
            Ok(false)
        },
    )?;
    let turn = turn.ok_or(Ineligible("actual turn absent/reverted"))?;
    eligible_ensure!(
        matches!(
            turn["status"].as_str(),
            Some("completed" | "failed" | "interrupted")
        ) && turn["completedAt"].is_number()
            && turn["reverted"] != true,
        "actual turn is not terminal"
    );
    let mut found = false;
    pages(
        request,
        "thread/items/list",
        json!({"threadId":e.thread_id,"turnId":actual,"limit":200,"sortDirection":"asc"}),
        |row| {
            ensure!(row["turnId"].is_string(), "malformed item page turn");
            eligible_ensure!(row["turnId"] == actual, "item belongs to another turn");
            if external_item(&row["item"], e)? {
                eligible_ensure!(!found, "duplicate external item");
                found = true;
            }
            Ok(found)
        },
    )?;
    eligible_ensure!(found, "actual turn lacks external_event");
    // Never hash/store transcript or output text in the evidence.
    Ok(
        json!({"id":actual,"status":turn["status"],"completedAt":turn["completedAt"],"externalItemId":e.message.id}),
    )
}

fn collect_with(
    e: &Envelope,
    mut request: impl FnMut(&str, Value) -> anyhow::Result<Value>,
    fence: impl FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<Observation> {
    collect_with_deadline(e, &mut request, fence, Instant::now() + OBSERVATION_BUDGET)
}

fn collect_with_deadline(
    e: &Envelope,
    mut request: impl FnMut(&str, Value) -> anyhow::Result<Value>,
    mut fence: impl FnMut() -> anyhow::Result<()>,
    deadline: Instant,
) -> anyhow::Result<Observation> {
    let bounded_request = |method: &str, params: Value| -> anyhow::Result<Value> {
        remaining(deadline)?;
        let result = request(method, params);
        remaining(deadline)?;
        // These are the audited adapter's explicit absence/binding-conflict
        // replies. A timeout, disconnect, generic RPC error, or malformed
        // protocol is never reinterpreted as a negative observation.
        if method != "claude/capabilities" {
            if let Err(error) = &result {
                if let Some(cutex::app_server::client::AppServerClientError::Rpc(rpc)) =
                    error.downcast_ref::<cutex::app_server::client::AppServerClientError>()
                {
                    if rpc.code == -32004
                        || (method == "thread/externalInput/status" && rpc.code == -32011)
                    {
                        return Err(Ineligible(
                            "adapter reports missing input/history or changed binding",
                        )
                        .into());
                    }
                }
            }
        }
        result
    };
    classify(collect_validated(e, bounded_request, || {
        remaining(deadline)?;
        fence()?;
        remaining(deadline)?;
        Ok(())
    }))
}

fn collect_validated(
    e: &Envelope,
    mut request: impl FnMut(&str, Value) -> anyhow::Result<Value>,
    mut fence: impl FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<Observation> {
    ensure!(
        matches!(e.version, 1 | 2),
        "unsupported frozen envelope version"
    );
    if let Some(view) = &e.view {
        view.canonical_json()?;
    }
    eligible_ensure!(
        e.semantic_sha256 == e.digest(),
        "frozen envelope digest conflict"
    );
    e.validate()?;
    if e.message.delivery == Delivery::Passive || e.message.event_type != "agent_message" {
        return Ok(Observation::Ineligible);
    }
    fence()?;
    let caps = request("claude/capabilities", json!({}))?;
    // Fence the implementation whose payload and UUID derivation we audited.
    // Future versions need an explicit compatibility review, not a >= guess.
    ensure!(
        caps["implementation"] == "cute-claude-app-server"
            && caps["version"] == "0.8.5"
            && super::super::claude_ingress::offers_native(&caps),
        "unsupported Claude settlement adapter"
    );
    let digest = local_digest(e)?;
    let before_native = native(&mut request, e)?;
    let before_local = local(&mut request, e, &digest)?;
    let actual = actual(e, &before_local)?;
    let attempt = attempt_id(&e.thread_id, actual);
    eligible_ensure!(
        before_native["statuses"][0]["processing"]["attemptId"] == attempt,
        "attempt/actual turn conflict"
    );
    let turn = history(&mut request, e, actual)?;
    eligible_ensure!(
        turn["status"] == before_local["state"],
        "local/terminal turn conflict"
    );
    let after_local = local(&mut request, e, &digest)?;
    let after_native = native(&mut request, e)?;
    eligible_ensure!(
        before_local == after_local && before_native == after_native,
        "receipt or input status changed during collection"
    );
    fence()?;
    Ok(Observation::Terminal(Evidence {
        observing_generation: None,
        schema_version: 1,
        runtime: "claude".into(),
        recipient_id: e.owner_id.clone(),
        runtime_generation: e.runtime_generation,
        thread_id: e.thread_id.clone(),
        message_id: e.message.id.clone(),
        semantic_sha256: e.semantic_sha256.clone(),
        local_semantic_sha256: digest,
        attempt_id: attempt,
        actual_turn_id: actual.into(),
        terminal_status: turn["status"].as_str().unwrap().into(),
        processing_state: after_native["statuses"][0]["processing"]["state"]
            .as_str()
            .context("processing state missing")?
            .into(),
        observed_at: Utc::now(),
        observation_sha256: hash(&json!({"native":after_native,"local":{
            "threadId":after_local["threadId"],"messageId":after_local["messageId"],
            "semanticSha256":after_local["semanticSha256"],"turnId":after_local["turnId"],
            "contextTurnId":after_local["contextTurnId"],"contextPersisted":after_local["contextPersisted"],
            "state":after_local["state"],"durable":after_local["durable"],"receiptScope":after_local["receiptScope"]
        },"turn":turn}))?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cutex::app_server::external_input::{Message, Source, SourceKind};

    struct Fixture {
        e: Envelope,
        native: Value,
        local: Value,
        thread: Value,
        caps: Value,
    }
    impl Fixture {
        fn new(soon: bool, terminal: &str, output: bool) -> Self {
            let mut e = Envelope {
                version: 2,
                owner_id: "recipient".into(),
                thread_id: "thread".into(),
                runtime_generation: 7,
                message: Message {
                    id: "message".into(),
                    source: Source {
                        kind: SourceKind::Agent,
                        id: "sender".into(),
                    },
                    event_type: "agent_message".into(),
                    delivery: if soon {
                        Delivery::Soon
                    } else {
                        Delivery::AfterTurn
                    },
                    text: "hello\n世界 🦀".into(),
                },
                view: Some(
                    serde_json::from_value(
                        json!({"schema":"cutex.agent-message.v1","data":{"senderName":"Sender"}}),
                    )
                    .unwrap(),
                ),
                semantic_sha256: String::new(),
            };
            e.semantic_sha256 = e.digest();
            let native = json!({"version":1,"ownerId":e.owner_id,"threadId":e.thread_id,"runtimeGeneration":7,
                "statuses":[{"messageId":e.message.id,"semanticSha256":e.semantic_sha256,"deliveryState":"pending","receipt":null,
                    "processing":{"state":if output {"output_observed"} else {"held"},"attemptId":attempt_id("thread","actual"),
                        "reason":if output {Value::Null} else {json!(match terminal {"interrupted"=>"interrupted","completed"=>"no_output",_=>"request_uncertain"})}}}]});
            let mut local = json!({"threadId":e.thread_id,"messageId":e.message.id,"turnId":if soon {"reserved"} else {"actual"},
                "semanticSha256":local_digest(&e).unwrap(),"state":terminal,"durable":true,"receiptScope":"local-inbox"});
            if soon {
                local["contextTurnId"] = json!("actual");
                local["contextPersisted"] = json!(false);
            }
            let item = json!({"type":"functionCallOutput","id":e.message.id,"name":"external_event",
                "output":serde_json::to_string(&json!({"source":e.message.source,"type":e.message.event_type,"text":e.message.text})).unwrap(),
                "externalInputView":e.view});
            let mut items = vec![item];
            if output {
                items.push(json!({"type":"agentMessage","id":"output","text":"partial"}));
            }
            let thread = json!({"id":e.thread_id,"modelProvider":"cute-claude","cliVersion":"0.8.5",
                "turns":[{"id":"actual","status":terminal,"completedAt":1234,"items":items,"itemsView":"full"}]});
            Self {
                e,
                native,
                local,
                thread,
                caps: json!({"implementation":"cute-claude-app-server","version":"0.8.5",
                "externalInputVersion":1,"externalInputVersions":[1,2],"externalInputDeliveries":["after_turn","passive","soon"]}),
            }
        }
        fn response(&self, method: &str) -> Value {
            match method {
                "claude/capabilities" => self.caps.clone(),
                "thread/externalInput/status" => self.native.clone(),
                "claude/input/status" => self.local.clone(),
                "thread/read" => {
                    json!({"thread":{"id":self.thread["id"],"modelProvider":self.thread["modelProvider"],
                    "cliVersion":self.thread["cliVersion"],"turns":[]}})
                }
                "thread/turns/list" => {
                    let data = self.thread["turns"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|turn| {
                            let mut turn = turn.clone();
                            turn["items"] = json!([]);
                            turn["itemsView"] = json!("notLoaded");
                            turn
                        })
                        .collect::<Vec<_>>();
                    json!({"data":data,"nextCursor":null})
                }
                "thread/items/list" => {
                    let data = self.thread["turns"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .flat_map(|turn| {
                            turn["items"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .map(move |item| json!({"turnId":turn["id"],"item":item}))
                        })
                        .collect::<Vec<_>>();
                    json!({"data":data,"nextCursor":null})
                }
                _ => panic!("unexpected RPC {method}; no start/submit/ensure is allowed"),
            }
        }
        fn run(&self) -> anyhow::Result<Observation> {
            collect_with(
                &self.e,
                |method, params| {
                    if method == "thread/read" {
                        assert_eq!(params["includeTurns"], false);
                    }
                    if method == "thread/turns/list" {
                        assert_eq!(params["itemsView"], "notLoaded");
                    }
                    if method == "thread/items/list" {
                        assert_eq!(params["turnId"], "actual");
                    }
                    Ok(self.response(method))
                },
                || Ok(()),
            )
        }
    }
    fn rejected(result: anyhow::Result<Observation>) {
        assert!(
            !matches!(result, Ok(Observation::Terminal(_))),
            "unexpected eligible observation"
        );
    }

    #[test]
    fn failed_no_output_and_interrupted_output_are_terminal_not_success() {
        for soon in [false, true] {
            for (status, output) in [
                ("failed", false),
                ("interrupted", true),
                ("completed", false),
                ("failed", true),
                ("completed", true),
            ] {
                let f = Fixture::new(soon, status, output);
                let Observation::Terminal(e) = f.run().unwrap() else {
                    panic!("terminal evidence expected")
                };
                assert_eq!(e.actual_turn_id, "actual");
                assert_eq!(e.terminal_status, status);
                assert_eq!(
                    e.processing_state,
                    if output { "output_observed" } else { "held" }
                );
                assert_ne!(e.semantic_sha256, e.local_semantic_sha256);
                assert_eq!(f.native["statuses"][0]["receipt"], Value::Null);
            }
        }
    }

    #[test]
    fn independent_python_probe_digest_and_attempt_vectors() {
        // Frozen offline Python Store/Engine probe, not computed by this validator.
        let mut f = Fixture::new(false, "interrupted", true);
        f.e.thread_id = "fbb85a1d-d119-4352-807e-f0c6f6a0a299".into();
        f.e.message.id = "fifo-interrupted-output1".into();
        f.e.message.text = "exact semantic input fifo-interrupted-output1".into();
        f.e.semantic_sha256 = f.e.digest();
        assert_eq!(
            f.e.semantic_sha256,
            "8572119564fcc1817ac6f0b020a519fd5d8864504589f122871be95baae7504d"
        );
        assert_eq!(
            local_digest(&f.e).unwrap(),
            "c6b2dd95246262ec14549aedddd104b9addd2935b6e511ff18b991b0de029ccb"
        );
        assert_eq!(
            attempt_id(&f.e.thread_id, "0aebd6b6-1c02-493c-a2cc-a542086f0633"),
            "4f74cf7d-14bb-5455-a8ed-e210984b85f8"
        );
    }

    #[test]
    fn held_to_output_observed_changes_evidence_identity() {
        let held = Fixture::new(false, "failed", false).run().unwrap();
        let output = Fixture::new(false, "failed", true).run().unwrap();
        let (Observation::Terminal(held), Observation::Terminal(output)) = (held, output) else {
            panic!("expected terminal observations")
        };
        assert!(!held.same_identity(&output));
        assert_ne!(held.observation_sha256, output.observation_sha256);
    }

    #[test]
    fn busy_never_falls_back_to_reservation() {
        for context in [Value::Null, json!(""), json!("reserved")] {
            let mut f = Fixture::new(true, "interrupted", true);
            f.local["contextTurnId"] = context;
            rejected(f.run());
        }
        // Even a matching attempt for the reservation is insufficient: it
        // must be the actual terminal turn containing this message's item.
        let mut f = Fixture::new(true, "failed", false);
        f.local["contextTurnId"] = json!("reserved");
        f.native["statuses"][0]["processing"]["attemptId"] =
            json!(attempt_id("thread", "reserved"));
        rejected(f.run());
    }

    #[test]
    fn untouched_soon_fifo_requires_terminal_history_and_exact_input_item() {
        let mut f = Fixture::new(true, "interrupted", true);
        f.local["contextTurnId"] = Value::Null;
        f.local["turnId"] = json!("actual");
        assert!(matches!(f.run().unwrap(), Observation::Terminal(_)));
        f.thread["turns"][0]["items"] = json!([]);
        rejected(f.run());
        f.thread["turns"][0]["status"] = json!("inProgress");
        rejected(f.run());
    }

    #[test]
    fn historical_message_query_uses_current_connection_without_rewriting_submission() {
        let mut f = Fixture::new(false, "failed", false);
        let original = f.e.clone();
        f.native["runtimeGeneration"] = json!(9);
        let result = collect_read_binding(
            &f.e,
            9,
            |method, params| {
                if method == "thread/externalInput/status" {
                    assert_eq!(params["runtimeGeneration"], 9);
                }
                Ok(f.response(method))
            },
            || Ok(()),
        )
        .unwrap();
        let Observation::Terminal(evidence) = result else {
            panic!("historical terminal expected")
        };
        assert_eq!(evidence.runtime_generation, 7);
        assert_eq!(evidence.observer_generation(), 9);
        assert_eq!(f.e, original);
        rejected(collect_read_binding(
            &f.e,
            10,
            |method, _| Ok(f.response(method)),
            || Ok(()),
        ));
    }

    #[test]
    fn rejects_native_identity_digest_attempt_and_generation_conflicts() {
        for (pointer, value) in [
            ("/ownerId", json!("wrong")),
            ("/threadId", json!("wrong")),
            ("/runtimeGeneration", json!(8)),
            ("/statuses/0/messageId", json!("wrong")),
            ("/statuses/0/semanticSha256", json!("0".repeat(64))),
            ("/statuses/0/processing/attemptId", Value::Null),
            (
                "/statuses/0/processing/attemptId",
                json!(uuid::Uuid::new_v4().to_string()),
            ),
            ("/statuses/0/deliveryState", json!("conflict")),
        ] {
            let mut f = Fixture::new(false, "failed", false);
            *f.native.pointer_mut(pointer).unwrap() = value;
            rejected(f.run());
        }
        let mut f = Fixture::new(false, "failed", false);
        f.e.message.text.push('!'); // Invalid frozen native semantic hash.
        rejected(f.run());
    }

    #[test]
    fn rejects_local_identity_digest_state_and_non_durable_receipts() {
        for (key, value) in [
            ("threadId", json!("wrong")),
            ("messageId", json!("wrong")),
            ("semanticSha256", json!("bad")),
            ("durable", json!(false)),
            ("receiptScope", json!("provider")),
            ("contextPersisted", json!(true)),
        ] {
            let mut f = Fixture::new(false, "failed", false);
            f.local[key] = value;
            rejected(f.run());
        }
        for state in [
            "queued",
            "pending",
            "processing",
            "injecting",
            "inProgress",
            "running",
            "reverted",
            "inherited",
            "uncertain",
        ] {
            let mut f = Fixture::new(false, "failed", false);
            f.local["state"] = json!(state);
            rejected(f.run());
        }
        let mut f = Fixture::new(false, "failed", false);
        f.local["semanticSha256"] = json!(f.e.semantic_sha256);
        rejected(f.run());
    }

    #[test]
    fn passive_pending_and_claimed_are_ineligible() {
        let mut f = Fixture::new(false, "failed", false);
        f.e.message.delivery = Delivery::Passive;
        f.e.semantic_sha256 = f.e.digest();
        assert!(matches!(
            collect_with(
                &f.e,
                |_, _| panic!("passive must not read RPC"),
                || panic!("no fence needed")
            ),
            Ok(Observation::Ineligible)
        ));
        for state in ["pending", "claimed", "none"] {
            let mut f = Fixture::new(false, "failed", false);
            f.native["statuses"][0]["processing"] = json!({"state":state,"attemptId":if state=="none" {Value::Null} else {json!(attempt_id("thread","actual"))},"reason":null});
            assert!(matches!(
                collect_with(
                    &f.e,
                    |method, _| {
                        assert!(
                            matches!(
                                method,
                                "claude/capabilities" | "thread/externalInput/status"
                            ),
                            "must reject before local/history reads"
                        );
                        Ok(f.response(method))
                    },
                    || Ok(())
                ),
                Ok(Observation::Ineligible)
            ));
        }
    }

    #[test]
    fn rejects_wrong_history_turn_item_payload_and_nonterminal() {
        for (pointer, value) in [
            ("/id", json!("other")),
            ("/turns/0/id", json!("reserved")),
            ("/turns/0/status", json!("inProgress")),
            ("/turns/0/status", json!("completed")),
            ("/turns/0/completedAt", Value::Null),
            ("/turns/0/items/0/id", json!("other")),
            ("/turns/0/items/0/name", json!("other")),
            ("/turns/0/items/0/type", json!("agentMessage")),
            ("/turns/0/items/0/output", json!("{}")),
            ("/turns/0/items/0/externalInputView", Value::Null),
        ] {
            let mut f = Fixture::new(false, "failed", false);
            *f.thread.pointer_mut(pointer).unwrap() = value;
            rejected(f.run());
        }
    }

    #[test]
    fn unavailable_or_changed_generation_fails_before_and_after_read() {
        let f = Fixture::new(false, "failed", false);
        assert!(collect_with(
            &f.e,
            |_, _| panic!("unavailable generation must not RPC"),
            || anyhow::bail!("unavailable"),
        )
        .is_err());
        let mut fences = 0;
        assert!(matches!(
            collect_with(
                &f.e,
                |m, _| Ok(f.response(m)),
                || {
                    fences += 1;
                    eligible_ensure!(fences == 1, "generation/thread changed");
                    Ok(())
                },
            ),
            Ok(Observation::Ineligible)
        ));
        assert_eq!(fences, 2);
        assert!(collect_with(&f.e, |_, _| anyhow::bail!("disconnected"), || Ok(()),).is_err());
    }

    #[test]
    fn receipt_revert_or_attempt_changes_during_observation_invalidate() {
        for change in ["receipt", "revert", "attempt", "local-binding"] {
            let f = Fixture::new(true, "interrupted", true);
            let (mut native_reads, mut local_reads) = (0, 0);
            assert!(matches!(
                collect_with(
                    &f.e,
                    |method, _| {
                        let mut raw = f.response(method);
                        if method == "thread/externalInput/status" {
                            native_reads += 1;
                            if native_reads == 2 {
                                if change == "receipt" {
                                    raw["statuses"][0]["deliveryState"] =
                                        json!("context_persisted");
                                    let mut receipt = cutex::app_server::external_input::Receipt {
                                        schema: "codex.external-input-receipt.v1".into(),
                                        receipt_id: String::new(),
                                        owner_id: f.e.owner_id.clone(),
                                        thread_id: f.e.thread_id.clone(),
                                        message_id: f.e.message.id.clone(),
                                        semantic_sha256: f.e.semantic_sha256.clone(),
                                        response_item_id: f.e.message.id.clone(),
                                        turn_id: "actual".into(),
                                        ordinal: 1,
                                    };
                                    receipt.receipt_id = receipt.digest_id();
                                    raw["statuses"][0]["receipt"] =
                                        serde_json::to_value(receipt).unwrap();
                                }
                                if change == "attempt" {
                                    raw["statuses"][0]["processing"]["attemptId"] =
                                        json!(uuid::Uuid::new_v4().to_string());
                                }
                            }
                        }
                        if method == "claude/input/status" {
                            local_reads += 1;
                            if local_reads == 2 {
                                if change == "revert" {
                                    raw["state"] = json!("reverted");
                                }
                                if change == "local-binding" {
                                    raw["contextTurnId"] = json!("reserved");
                                }
                            }
                        }
                        Ok(raw)
                    },
                    || Ok(()),
                ),
                Ok(Observation::Ineligible)
            ));
        }
    }

    #[test]
    fn only_audited_adapter_version_is_accepted() {
        for (key, value) in [
            ("version", json!("0.8.6")),
            ("implementation", json!("another-adapter")),
            ("externalInputVersion", json!(0)),
        ] {
            let mut f = Fixture::new(false, "failed", false);
            f.caps[key] = value;
            assert!(f.run().is_err());
        }
    }

    #[test]
    fn metadata_only_read_then_paginated_target_turn_and_items() {
        let f = Fixture::new(true, "interrupted", true);
        let (mut turn_pages, mut item_pages) = (0, 0);
        let result = collect_with(
            &f.e,
            |method, params| {
                let mut raw = f.response(method);
                match method {
                    "thread/read" => raw["thread"]["turns"] = json!([]),
                    "thread/turns/list" => {
                        assert_eq!(params["itemsView"], "notLoaded");
                        turn_pages += 1;
                        raw = if turn_pages == 1 {
                            json!({"data":[],"nextCursor":"turn-page2"})
                        } else {
                            assert_eq!(params["cursor"], "turn-page2");
                            let mut turn = f.thread["turns"][0].clone();
                            turn["items"] = json!([]);
                            json!({"data":[turn],"nextCursor":null})
                        };
                    }
                    "thread/items/list" => {
                        assert_eq!(params["turnId"], "actual");
                        item_pages += 1;
                        raw = if item_pages == 1 {
                            json!({"data":[],"nextCursor":"item-page2"})
                        } else {
                            assert_eq!(params["cursor"], "item-page2");
                            json!({"data":[{"turnId":"actual","item":f.thread["turns"][0]["items"][0]}],"nextCursor":null})
                        };
                    }
                    _ => {}
                }
                Ok(raw)
            },
            || Ok(()),
        );
        assert!(matches!(result, Ok(Observation::Terminal(_))));
        assert_eq!((turn_pages, item_pages), (2, 2));
    }

    #[test]
    fn cyclic_or_exhausted_history_is_not_evidence() {
        for next in [json!("cycle"), Value::Null] {
            let f = Fixture::new(false, "failed", false);
            rejected(collect_with(
                &f.e,
                |method, _| {
                    if method == "thread/read" {
                        let mut raw = f.response(method);
                        raw["thread"]["turns"] = json!([]);
                        return Ok(raw);
                    }
                    if method == "thread/turns/list" {
                        return Ok(json!({"data":[],"nextCursor":next}));
                    }
                    Ok(f.response(method))
                },
                || Ok(()),
            ));
        }
    }

    #[test]
    fn receipt_or_changed_frozen_snapshot_is_ineligible_without_rpc() {
        let f = Fixture::new(false, "failed", false);
        let mut snapshot: AgentBusMessageSnapshot = serde_json::from_value(json!({
            "messageId":f.e.message.id,"toCutexSessionId":f.e.owner_id,"deliveryMode":"after-turn",
            "content":f.e.message.text,"state":"inbox_accepted","externalInput":f.e
        }))
        .unwrap();
        assert!(eligible(&snapshot, &f.e));
        snapshot.state = "context_injected".into();
        assert!(matches!(
            collect(&snapshot).unwrap(),
            Observation::Ineligible
        ));
        snapshot.state = "inbox_accepted".into();
        snapshot.to_cutex_session_id = "another-recipient".into();
        assert!(matches!(
            collect(&snapshot).unwrap(),
            Observation::Ineligible
        ));
    }

    #[test]
    fn wrong_paged_item_turn_and_exhausted_read_budget_are_rejected() {
        for wrong_turn in [true, false] {
            let f = Fixture::new(false, "failed", false);
            let mut pages = 0;
            rejected(collect_with(
                &f.e,
                |method, _| {
                    if method == "thread/items/list" {
                        pages += 1;
                        return Ok(if wrong_turn {
                            json!({"data":[{"turnId":"reserved","item":f.thread["turns"][0]["items"][0]}],"nextCursor":null})
                        } else {
                            json!({"data":[],"nextCursor":format!("cursor-{pages}")})
                        });
                    }
                    Ok(f.response(method))
                },
                || Ok(()),
            ));
            assert_eq!(pages, if wrong_turn { 1 } else { MAX_PAGES });
        }
    }

    #[test]
    fn known_identity_state_and_generation_conflicts_are_ineligible_not_unavailable() {
        for (pointer, value) in [
            ("/ownerId", json!("other")),
            ("/runtimeGeneration", json!(8)),
            ("/statuses/0/messageId", json!("other")),
            ("/statuses/0/semanticSha256", json!("0".repeat(64))),
            ("/statuses/0/deliveryState", json!("conflict")),
            (
                "/statuses/0/processing/attemptId",
                json!(uuid::Uuid::new_v4().to_string()),
            ),
        ] {
            let mut f = Fixture::new(false, "failed", false);
            *f.native.pointer_mut(pointer).unwrap() = value;
            assert!(matches!(f.run(), Ok(Observation::Ineligible)), "{pointer}");
        }
        for (key, value) in [
            ("state", json!("running")),
            ("state", json!("queued")),
            ("state", json!("reverted")),
            ("contextPersisted", json!(true)),
            ("semanticSha256", json!("wrong")),
            ("threadId", json!("other")),
        ] {
            let mut f = Fixture::new(false, "failed", false);
            f.local[key] = value;
            assert!(matches!(f.run(), Ok(Observation::Ineligible)), "{key}");
        }
        let mut f = Fixture::new(true, "failed", false);
        f.local["contextTurnId"] = Value::Null;
        assert!(matches!(f.run(), Ok(Observation::Ineligible)));
        let mut f = Fixture::new(false, "failed", false);
        f.thread["turns"][0]["status"] = json!("inProgress");
        assert!(matches!(f.run(), Ok(Observation::Ineligible)));
    }

    #[test]
    fn malformed_responses_remain_errors() {
        for method in [
            "claude/capabilities",
            "thread/externalInput/status",
            "claude/input/status",
            "thread/read",
        ] {
            let f = Fixture::new(false, "failed", false);
            assert!(
                collect_with(
                    &f.e,
                    |m, _| Ok(if m == method {
                        json!({})
                    } else {
                        f.response(m)
                    }),
                    || Ok(())
                )
                .is_err(),
                "{method}"
            );
        }
    }

    #[test]
    fn explicit_adapter_absence_or_binding_conflict_differs_from_rpc_failure() {
        for (code, ineligible) in [
            (-32004, true),
            (-32011, true),
            (-32603, false),
            (-32601, false),
        ] {
            let f = Fixture::new(false, "failed", false);
            let result = collect_with(
                &f.e,
                |method, _| {
                    if method == "thread/externalInput/status" {
                        return Err(cutex::app_server::client::AppServerClientError::Rpc(
                            cutex::app_server::protocol::RpcError {
                                code,
                                message: "fixture".into(),
                                data: None,
                            },
                        )
                        .into());
                    }
                    Ok(f.response(method))
                },
                || Ok(()),
            );
            if ineligible {
                assert!(matches!(result, Ok(Observation::Ineligible)));
            } else {
                assert!(result.is_err());
            }
        }
    }

    #[test]
    fn expired_total_deadline_makes_no_rpc_and_late_response_stops_further_reads() {
        let f = Fixture::new(false, "failed", false);
        assert!(collect_with_deadline(
            &f.e,
            |_, _| panic!("expired budget must not RPC"),
            || Ok(()),
            Instant::now() - Duration::from_secs(1)
        )
        .is_err());
        let deadline = Instant::now() + Duration::from_millis(2);
        let mut reads = 0;
        assert!(collect_with_deadline(
            &f.e,
            |method, _| {
                reads += 1;
                // Simulate one response reaching the shared deadline. No model,
                // provider, sleeping worker, or second RPC is involved.
                while Instant::now() <= deadline {
                    std::thread::yield_now();
                }
                Ok(f.response(method))
            },
            || Ok(()),
            deadline
        )
        .is_err());
        assert!(reads <= 1);
    }

    #[test]
    fn background_rpc_timeout_uses_remaining_shared_deadline_and_owner_budget_is_independent() {
        let f = Fixture::new(true, "interrupted", true);
        let deadline = Instant::now() + Duration::from_millis(2);
        let mut reads = 0;
        let frozen = f.e.clone();
        assert!(collect_read_binding_with_deadline(
            &f.e,
            7,
            |method, _, timeout| {
                reads += 1;
                assert!(timeout <= Duration::from_millis(2));
                assert!(!timeout.is_zero());
                while Instant::now() <= deadline {
                    std::thread::yield_now();
                }
                Ok(f.response(method))
            },
            || Ok(()),
            deadline
        )
        .is_err());
        assert!(
            reads <= 1,
            "late replies cannot start another native/history read"
        );
        assert_eq!(f.e, frozen);
        assert!(collect_background(&serde_json::from_value(json!({"messageId":"message","toCutexSessionId":"recipient",
            "deliveryMode":"soon","content":"synthetic","state":"inbox_accepted","externalInput":f.e})).unwrap(),
            Instant::now()-Duration::from_secs(1)).is_err());

        // Same full-proof collector, but a fresh owner budget instead of the
        // exhausted background budget. No shorter global state contaminates it.
        let mut timeouts = Vec::new();
        let result = collect_read_binding_with_deadline(
            &f.e,
            7,
            |method, _, timeout| {
                timeouts.push(timeout);
                Ok(f.response(method))
            },
            || Ok(()),
            Instant::now() + OBSERVATION_BUDGET,
        )
        .unwrap();
        let Observation::Terminal(evidence) = result else {
            panic!("full owner proof expected");
        };
        assert_eq!(OBSERVATION_BUDGET, Duration::from_secs(15));
        assert_eq!(timeouts[0], Duration::from_secs(10));
        assert!(timeouts
            .iter()
            .all(|timeout| *timeout > background::MESSAGE_BUDGET));
        assert_eq!(evidence.runtime_generation, 7);
        assert_eq!(evidence.observing_generation, Some(7));
        assert_eq!(evidence.actual_turn_id, "actual");
        assert_eq!(f.e, frozen);
    }

    #[test]
    fn real_sidecar_write_fault_and_recovery_do_not_prevent_ordinary_bus_polls() {
        use cutex::management::v2::agent_bus_state::{
            AgentBusMessageRepository, AgentBusQueuedMessage,
        };
        use std::cell::Cell;
        // Parent runs bin tests with its marked owner-private HOME; fixture
        // data and the synthetic HTTP server never touch a live daemon/runtime.
        let private = std::path::PathBuf::from(
            std::env::var_os("CUTEX_TEST_PRIVATE_HOME").expect("private HOME required"),
        );
        assert!(private.join(".cutex-test-private-home").is_file());
        assert_eq!(
            std::env::var_os("HOME"),
            Some(private.clone().into_os_string())
        );
        let root = std::env::temp_dir().join(format!(
            "claude-settlement-fault-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let repository = AgentBusMessageRepository::open(&root).unwrap();
        let f = Fixture::new(false, "interrupted", true);
        let canonical=serde_json::from_value(json!({"id":f.e.message.id,"from":"sender","to":"recipient",
            "fromCutexSessionId":"sender","toCutexSessionId":"recipient","content":f.e.message.text,
            "deliveryMode":"after_turn","triggerTurn":true,"createdAtEpochSecs":1,"senderKind":"agent"})).unwrap();
        repository
            .record_queued(AgentBusQueuedMessage {
                owner_cutex_session_id: f.e.owner_id.clone(),
                message_id: f.e.message.id.clone(),
                from_cutex_session_id: Some("sender".into()),
                to_cutex_session_id: f.e.owner_id.clone(),
                from_runtime_agent_id: Some("synthetic-from".into()),
                to_runtime_agent_id: Some("synthetic-to".into()),
                delivery_mode: "after_turn".into(),
                content: f.e.message.text.clone(),
                queued_at: Utc::now(),
                canonical_envelope: canonical,
                semantic_sha256: "c".repeat(64),
            })
            .unwrap();
        repository
            .freeze_external_input(&f.e.owner_id, &f.e.message.id, |_| Ok(f.e.clone()))
            .unwrap();
        let mut local = f.local.clone();
        local["state"] = json!("queued");
        repository
            .record_local_inbox(&f.e.owner_id, &f.e.message.id, &local)
            .unwrap();
        let status: Response = serde_json::from_value(f.native.clone()).unwrap();
        repository
            .observe_external_input(&f.e.message.id, &f.e, &status.statuses[0])
            .unwrap();
        let before = repository
            .snapshot_by_message_id(&f.e.message.id)
            .unwrap()
            .unwrap();
        // The bin test uses SQL against its own fixture, not the library's
        // crate-private hot_store implementation.
        let database = root.join("management-state.sqlite3");
        let db = rusqlite::Connection::open(&database).unwrap();
        let generation = || {
            db.query_row(
                "SELECT generation FROM hot_store_meta WHERE store='agent_bus'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
        };
        let business_generation = generation();
        db.execute_batch("CREATE TRIGGER synthetic_settlement_fault BEFORE INSERT ON hot_store_rows
            WHEN NEW.store='agent_bus_settlement_evidence' BEGIN SELECT RAISE(FAIL,'synthetic sidecar write failure'); END;").unwrap();

        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for stream in listener.incoming().take(4) {
                let mut stream = stream.unwrap();
                cutex::http::server::read_simple_http_request(&mut stream).unwrap();
                cutex::http::server::write_json_response(
                    &mut stream,
                    200,
                    "OK",
                    &json!({"messages":[]}),
                )
                .unwrap();
            }
        });
        let bus =
            cutex::agent_bus::client::AgentBusHttpClient::new(format!("http://{address}"), None);
        let mut schedule = BackgroundObservations::default();
        let polls = Cell::new(0);
        let writes_failed = Cell::new(0);
        let collected = Cell::new(0);
        for phase in 0..2 {
            if phase == 1 {
                db.execute_batch("DROP TRIGGER synthetic_settlement_fault;")
                    .unwrap();
            }
            schedule.after_deliveries(
                true,
                || {
                    for runtime in ["synthetic-runtime-1", "synthetic-runtime-2"] {
                        assert!(bus.poll(runtime).unwrap().is_empty());
                        polls.set(polls.get() + 1);
                    }
                    vec![f.e.owner_id.clone(), "unavailable-agent".into()]
                },
                |id| {
                    if id == f.e.owner_id {
                        Ok(vec![f.e.message.id.clone()])
                    } else {
                        anyhow::bail!("synthetic source unavailable")
                    }
                },
                |id, message, deadline| {
                    assert_eq!(
                        polls.get(),
                        (phase + 1) * 2,
                        "all polls precede collection/persistence"
                    );
                    super::super::observe_settlement_message_with(
                        &repository,
                        id,
                        message,
                        deadline,
                        |snapshot, deadline| {
                            collected.set(collected.get() + 1);
                            assert_eq!(snapshot.external_input.as_ref(), Some(&f.e));
                            collect_read_binding_with_deadline(
                                &f.e,
                                7,
                                |method, _, timeout| {
                                    assert!(timeout <= background::MESSAGE_BUDGET);
                                    Ok(f.response(method))
                                },
                                || Ok(()),
                                deadline,
                            )
                        },
                    )
                    .unwrap(); // real production F1 boundary swallows sidecar error
                    let retained = repository
                        .receipt_read_inputs(&[message.to_owned()])
                        .unwrap();
                    if retained[message].terminal.is_none() {
                        writes_failed.set(writes_failed.get() + 1);
                    }
                },
            );
            assert_eq!(
                repository.snapshot_by_message_id(&f.e.message.id).unwrap(),
                Some(before.clone())
            );
            assert_eq!(generation(), business_generation);
        }
        server.join().unwrap();
        assert_eq!(polls.get(), 4);
        assert_eq!(collected.get(), 2);
        assert_eq!(writes_failed.get(), 1);
        let retained = repository
            .receipt_read_inputs(&[f.e.message.id.clone()])
            .unwrap();
        let evidence = retained[&f.e.message.id]
            .terminal
            .as_ref()
            .expect("same-generation recovery retained full proof");
        assert_eq!(evidence.runtime_generation, 7);
        assert_eq!(evidence.observing_generation, Some(7));
        assert!(before.external_input_receipt.is_none());
    }
}
