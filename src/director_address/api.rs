use super::management::{self as mg, ExtensionStateStore};
use super::*;
use serde_json::{json, Value};

fn string<'a>(args: &'a Value, key: &str) -> Result<&'a str, AskError> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AskError::new("invalid_request", format!("{key} is required")))
}
fn number(args: &Value, key: &str) -> Result<u64, AskError> {
    args.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| AskError::new("invalid_request", format!("{key} is required")))
}
pub fn replacement_hint(old: &str, caller: Option<&str>) -> Result<Option<Value>, AskError> {
    let ext = mg::ManagementStore::open_default()
        .map_err(store::unavailable)?
        .read()
        .map_err(store::unavailable)?;
    let roster = crate::memory::load_agent_management_snapshot().map_err(store::unavailable)?;
    let config = crate::config::store::load_codez_config_checked().map_err(store::unavailable)?;
    let boundary =
        crate::agent_bus::boundary::AgentBoundary::from_snapshot(&config.agent_security, &roster);
    let record = ext
        .transitions
        .values()
        .filter(|t| {
            t.old_director.as_ref().is_some_and(|id| id.as_str() == old)
                && t.reason == mg::ChangeReason::ReplacementConfirmed
                && t.replacement_action_id.is_some()
                && matches!(
                    t.state,
                    mg::TransitionState::Finalized | mg::TransitionState::Superseded
                )
        })
        .filter_map(|t| {
            roster
                .projects
                .get(&t.project_id)
                .map(|authority| (t, authority))
        })
        .filter(|(_, authority)| {
            caller.is_none_or(|caller| {
                boundary.may_message(caller, authority.authorized_director_session.as_str())
            })
        })
        .max_by_key(|(t, _)| t.new_epoch);
    Ok(record.map(|(change,current)|json!({"projectId":change.project_id,"previousDirector":old,"currentDirector":current.authorized_director_session,"authorityEpoch":current.authority_epoch,"changeId":change.change_id,"targetRef":{"kind":"projectDirector","projectId":change.project_id},"automaticForwarding":false})))
}
pub fn reviews_for(caller: &str) -> Result<Value, AskError> {
    let roster = crate::memory::load_agent_management_snapshot().map_err(store::unavailable)?;
    let projects: std::collections::BTreeSet<_> = roster
        .projects
        .values()
        .filter(|a| {
            a.authorized_director_session.as_str() == caller
                && crate::memory::project_is_known(&roster, &a.project_id)
        })
        .map(|a| a.project_id.as_str())
        .collect();
    if projects.is_empty() {
        return Ok(json!({"handover":[],"start":[],"changes":[]}));
    }
    let ownership = Store::open_default()?.snapshot()?;
    let extension = mg::ManagementStore::open_default()
        .map_err(store::unavailable)?
        .read()
        .map_err(store::unavailable)?;
    let handover: Vec<_> = ownership
        .reviews
        .values()
        .filter(|r| projects.contains(r.project_id.as_str()) && r.to == caller)
        .collect();
    let start: Vec<_> = extension
        .start_reviews
        .values()
        .filter(|r| projects.contains(r.project_id.as_str()))
        .collect();
    let changes: Vec<_> = extension
        .transitions
        .values()
        .filter(|r| projects.contains(r.project_id.as_str()))
        .collect();
    Ok(json!({"handover":handover,"start":start,"changes":changes}))
}
pub fn review_handover(caller: &str, args: &Value) -> Result<Value, AskError> {
    review_handover_as(Some(caller), None, args)
}
pub(crate) fn review_handover_as(
    caller: Option<&str>,
    project: Option<&str>,
    args: &Value,
) -> Result<Value, AskError> {
    let id = string(args, "review_id")?;
    let revision = number(args, "expected_revision")?;
    let epoch = number(args, "expected_epoch")?;
    let accept = decision(args)?;
    let roster_store = crate::agent_management::AgentManagementStore::open_default()
        .map_err(store::unavailable)?;
    let _fence = roster_store.lock_mutations().map_err(store::unavailable)?;
    let roster = roster_store.snapshot().map_err(store::unavailable)?;
    let store = Store::open_default()?;
    let before = store.snapshot()?;
    let review = before
        .reviews
        .get(id)
        .ok_or_else(|| AskError::new("review_not_found", "Handover review does not exist"))?;
    if project.is_some_and(|p| p != review.project_id) {
        return Err(AskError::new(
            "review_not_found",
            "Review belongs to another project",
        ));
    }
    let (current, authority_epoch) = project_director_state(&roster, &review.project_id)?;
    if authority_epoch != epoch {
        return Err(AskError::new(
            "authority_conflict",
            "Director epoch changed",
        ));
    }
    let asks = crate::asks::store::AskStore::open_default().map_err(store::unavailable)?;
    let snapshot = asks.lookup_snapshot(&[&review.ask_id]).map_err(store::unavailable)?;
    let ask = snapshot
        .asks
        .get(&review.ask_id)
        .ok_or_else(|| AskError::new("ask_not_found", "Delegation no longer exists"))?;
    let director = current.as_deref().unwrap_or("");
    store.decide_review(
        id,
        revision,
        caller.unwrap_or("owner"),
        accept,
        ask,
        &review.project_id,
        director,
        epoch,
        caller.is_none(),
        successor_may_control(ask, director),
    )?;
    if accept {
        let after = store.snapshot()?;
        super::handover::apply_committed_handover(
            &asks,
            &after.asks[&ask.ask_id],
            after.asks[&ask.ask_id].summary_from.as_deref(),
        )?;
        store.finish_summary(&after.asks[&ask.ask_id])?;
    }
    Ok(json!({"ok":true,"item":store.snapshot()?.reviews[id]}))
}
fn decision(args: &Value) -> Result<bool, AskError> {
    match string(args, "decision")? {
        "accept" => Ok(true),
        "discard" => Ok(false),
        _ => Err(AskError::new(
            "invalid_request",
            "decision must be accept or discard",
        )),
    }
}

pub fn review_start(caller: &str, args: &Value) -> Result<Value, AskError> {
    review_start_as(Some(caller), None, args)
}
pub fn read_start(caller: &str, args: &Value) -> Result<Value, AskError> {
    let id = string(args, "review_id")?;
    let offset = args
        .get("offset")
        .map(|v| {
            v.as_u64().ok_or_else(|| {
                AskError::new("invalid_request", "offset must be a nonnegative integer")
            })
        })
        .transpose()?
        .unwrap_or(0);
    let roster_store = crate::agent_management::AgentManagementStore::open_default()
        .map_err(store::unavailable)?;
    let _fence = roster_store.lock_mutations().map_err(store::unavailable)?;
    let roster = roster_store.snapshot().map_err(store::unavailable)?;
    let ext = mg::ManagementStore::open_default()
        .map_err(store::unavailable)?
        .read()
        .map_err(store::unavailable)?;
    let review = ext
        .start_reviews
        .get(id)
        .ok_or_else(|| AskError::new("start_review_not_found", "Start review does not exist"))?;
    let authority = roster
        .projects
        .get(&review.project_id)
        .filter(|a| a.authorized_director_session.as_str() == caller)
        .ok_or_else(|| {
            AskError::new(
                "start_review_not_found",
                "Start review is not in the caller's current project",
            )
        })?;
    if !successor_may_target(review.target_session.as_str(), caller) {
        return Err(AskError::new(
            "handover_review_required",
            "Owner must approve access to the original start instructions",
        ));
    }
    let content = start_content(review.project_id.as_str(), id)?;
    let text = content["content"].as_str().expect("content string");
    let offset = usize::try_from(offset)
        .ok()
        .filter(|o| *o <= text.len() && text.is_char_boundary(*o))
        .ok_or_else(|| {
            AskError::new(
                "invalid_request",
                "offset must be a UTF-8 byte boundary within the instructions",
            )
        })?;
    let page = crate::asks::byte_prefix(&text[offset..], 4096);
    let end = offset + page.len();
    Ok(
        json!({"reviewId":id,"content":page,"offset":offset,"nextCursor":if end<text.len(){Some(end)}else{None},"totalBytes":text.len(),"authorityEpoch":authority.authority_epoch,"delivery":content["delivery"]}),
    )
}
pub(crate) fn review_start_as(
    caller: Option<&str>,
    project: Option<&str>,
    args: &Value,
) -> Result<Value, AskError> {
    let id = string(args, "review_id")?;
    let revision = number(args, "expected_revision")?;
    let epoch = number(args, "expected_epoch")?;
    let accept = decision(args)?;
    let roster_store = crate::agent_management::AgentManagementStore::open_default()
        .map_err(store::unavailable)?;
    let _fence = roster_store.lock_mutations().map_err(store::unavailable)?;
    let roster = roster_store.snapshot().map_err(store::unavailable)?;
    let store = mg::ManagementStore::open_default().map_err(store::unavailable)?;
    let before = store.read().map_err(store::unavailable)?;
    let review = before
        .start_reviews
        .get(id)
        .ok_or_else(|| AskError::new("start_review_not_found", "Start review does not exist"))?;
    if project.is_some_and(|p| p != review.project_id.as_str()) {
        return Err(AskError::new(
            "start_review_not_found",
            "Review belongs to another project",
        ));
    }
    if let Some(caller) = caller {
        if accept && !successor_may_target(review.target_session.as_str(), caller) {
            return Err(AskError::new(
                "handover_review_required",
                "Owner approval is required for the current permissions or boundary",
            ));
        }
    }
    let decision = mg::ReviewDecision {
        action_id: args
            .get("action_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| {
                format!(
                    "review:{id}:{revision}:{epoch}:{}:{}",
                    caller.unwrap_or("owner"),
                    if accept { "accept" } else { "discard" }
                )
            }),
        review_id: id.into(),
        expected_revision: revision,
        expected_epoch: epoch,
        actor: match caller {
            Some(id) => mg::ReviewActor::Session(
                crate::role_revision::CutexSessionId::new(id).map_err(store::unavailable)?,
            ),
            None => mg::ReviewActor::Owner,
        },
        reason: args
            .get("reason")
            .and_then(Value::as_str)
            .map(str::to_owned),
    };
    let mut result = None;
    // Persist a DeliveryUnknown classification even when approval is refused.
    let mut refusal = None;
    let unknown = if accept {
        source_unknown(review)?
    } else {
        false
    };
    store
        .update(&mut |ext| {
            let outcome = if accept {
                mg::approve_start_review(ext, &decision, &roster, unknown)
                    .map(|receipt| json!({"ok":true,"receipt":receipt}))
            } else {
                mg::discard_start_review(ext, &decision, &roster)
                    .map(|item| json!({"ok":true,"item":item}))
            };
            match outcome {
                Ok(value) => result = Some(value),
                Err(code) => refusal = Some(code),
            };
            Ok(true)
        })
        .map_err(store::unavailable)?;
    if let Some(code) = refusal {
        return Err(AskError::new(
            code,
            "Start review decision could not be applied",
        ));
    }
    Ok(result.expect("outcome"))
}
fn source_unknown(review: &mg::StartReviewItem) -> Result<bool, AskError> {
    let repository = crate::management::v2::agent_bus_state::agent_bus_message_repository()
        .map_err(store::unavailable)?;
    let messages = repository
        .management_start_messages()
        .map_err(store::unavailable)?;
    let Some(source) = crate::agent_management::first_message::find(
        &messages,
        &review.source_message_id,
        review.target_session.as_str(),
    ) else {
        return Ok(true);
    };
    let report = crate::agent_management::first_message::report(source);
    if report["outcome"] == "owner_discarded" {
        return Err(AskError::new(
            "source_discarded",
            "Owner discarded the original message",
        ));
    }
    Ok(report["outcome"] != "not_delivered")
}
pub(crate) fn start_content(project: &str, id: &str) -> Result<Value, AskError> {
    let ext = mg::ManagementStore::open_default()
        .map_err(store::unavailable)?
        .read()
        .map_err(store::unavailable)?;
    let item = ext
        .start_reviews
        .get(id)
        .filter(|r| r.project_id.as_str() == project)
        .ok_or_else(|| AskError::new("start_review_not_found", "Start review not found"))?;
    let repo = crate::management::v2::agent_bus_state::agent_bus_message_repository()
        .map_err(store::unavailable)?;
    let messages = repo
        .management_start_messages()
        .map_err(store::unavailable)?;
    let source = crate::agent_management::first_message::find(
        &messages,
        &item.source_message_id,
        item.target_session.as_str(),
    )
    .ok_or_else(|| AskError::new("source_delivery_unknown", "Original message is unavailable"))?;
    let message = repo
        .canonical_message(&source.message_id)
        .map_err(store::unavailable)?;
    if mg::content_sha256(&message.content) != item.content_sha256 {
        return Err(AskError::new(
            "director_provenance_invalid",
            "Original content digest changed",
        ));
    }
    Ok(
        json!({"reviewId":id,"content":message.content,"delivery":crate::agent_management::first_message::report(source)}),
    )
}
