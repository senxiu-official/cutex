//! Authoritative, scoped peer addressing. Cached directory rows are display
//! hints only: even a successful cached name hit must never bind a recipient.
use super::{
    store::{Peer, Store},
    Error, Result,
};
use crate::agent_names::{NameResolution, NameResolutionRequest, NameScope, ResolutionFailure};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const ROUTE: &str = "/peer/v1/agents/resolve";
pub const FEATURE: &str = crate::agent_names::FEATURE;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "camelCase", deny_unknown_fields)]
pub enum Reply {
    Resolved { resolution: NameResolution },
    Refused { failure: ResolutionFailure },
}

pub fn full_session_id(value: &str) -> bool {
    value
        .strip_prefix("cutex.")
        .is_some_and(|id| id.len() == 36 && uuid::Uuid::parse_str(id).is_ok())
}

pub fn validate(request: &NameResolutionRequest) -> Result<()> {
    if request.target.is_empty() || request.target.trim() != request.target
        || request.target.chars().count() > 128
        || request.target.chars().any(char::is_control)
        || request.target.eq_ignore_ascii_case("director")
        || request.scope.as_ref().is_some_and(|s| matches!(s, NameScope::Project { project_id }
            if project_id.is_empty() || project_id.len() > 128 || project_id.chars().any(char::is_control)))
    {
        return Err(Error::new("invalid_request", 400));
    }
    Ok(())
}

/// A structured refusal travels in the negotiated successful DTO rather than
/// adding arbitrary fields to old transport errors or old strict receipts.
pub fn reply(result: std::result::Result<NameResolution, ResolutionFailure>) -> Reply {
    match result {
        Ok(resolution) => Reply::Resolved { resolution },
        Err(failure) => Reply::Refused { failure },
    }
}

pub fn receive(peer: &Peer, data: Value) -> Result<Value> {
    if !super::pair::features().iter().any(|f| f == FEATURE) {
        return Err(Error::new("not_found", 404));
    }
    let request: NameResolutionRequest =
        serde_json::from_value(data).map_err(|_| Error::new("invalid_request", 400))?;
    validate(&request)?;
    let visible = || -> Result<Vec<String>> {
        let current = Store::open()?.get(&peer.peer_id)?;
        let directory = crate::asks::directory::Directory::load()
            .map_err(|_| Error::new("persistence_unavailable", 503))?;
        Ok(directory
            .agents
            .values()
            .filter(|a| super::policy::visible(&current, a))
            .map(|a| a.cutex_session_id.clone())
            .collect())
    };
    for _ in 0..2 {
        let visible_ids = visible()?;
        // This adapter is the only coupling to the authoritative ledger. Its
        // callback intersects IDs and fences the name/roster revisions. Recheck
        // the peer boundary too: a project move or export-policy edit between
        // the initial visibility read and that callback must not leak a match.
        let resolution =
            crate::asks::directory::Directory::resolve_name_for_visible(&request, &visible_ids)
                .map_err(|_| Error::new("persistence_unavailable", 503))?;
        if visible_ids == visible()? {
            return Ok(serde_json::to_value(reply(resolution))?);
        }
    }
    Err(Error::new("peer_unavailable", 409))
}

fn refusal(failure: ResolutionFailure, label: &str) -> Error {
    let code = match failure {
        ResolutionFailure::NotFound => "id_not_found",
        ResolutionFailure::Ambiguous { .. } => "id_ambiguous",
        ResolutionFailure::Archived { .. } => "recipient_archived",
        ResolutionFailure::Retired { .. } => "recipient_retired",
    };
    let mut error = Error::new(code, if code == "id_not_found" { 404 } else { 409 });
    error.details = json!({"host":label,"resolutionFailure":failure});
    if let ResolutionFailure::Ambiguous { candidates } = &failure {
        error.details["candidates"] = json!(candidates
            .iter()
            .map(|c| json!({
                "id":format!("{}@{label}",c.cutex_session_id),
                "name":super::wire::sanitize_name(&c.current_name),"scope":c.scope,
                "archived":c.archived,"retired":c.retired
            }))
            .collect::<Vec<_>>());
    }
    error
}

pub fn unsupported(label: &str) -> Error {
    let mut error = Error::new("peer_name_resolution_unsupported", 409);
    error.details = json!({"host":label,"missingFeatures":[FEATURE],
        "detail":format!("Host {label} does not support scoped agent-name resolution. Use the full cutex.<UUID>@{label} session ID from agent_list; cached names and short IDs are not safe for this peer.")});
    error
}

/// Always probe the remote authority first. No transport, downgrade or name
/// miss falls back to a previously successful cached alias.
pub fn resolve(
    store: &Store,
    peer: &Peer,
    request: &NameResolutionRequest,
) -> Result<(Peer, Value)> {
    validate(request)?;
    let peer = super::routes::refresh_capabilities(store, peer)?;
    if !peer.negotiated_features.iter().any(|f| f == FEATURE) {
        if !full_session_id(&request.target) {
            return Err(unsupported(&peer.label));
        }
        // An immutable ID may be sent to an older peer. The receiver performs
        // its existing visibility/lifecycle admission gate; no cache is read.
        return Ok((
            peer,
            json!({"cutexSessionId":request.target,"name":request.target}),
        ));
    }
    let value = super::client::call(
        store,
        &peer,
        "POST",
        ROUTE,
        Some(&serde_json::to_value(request)?),
    )?;
    let response: Reply =
        serde_json::from_value(value).map_err(|_| Error::new("peer_protocol_error", 502))?;
    let resolution = match response {
        Reply::Resolved { resolution } => resolution,
        Reply::Refused { failure } => return Err(refusal(failure, &peer.label)),
    };
    if !full_session_id(&resolution.cutex_session_id)
        || (full_session_id(&request.target) && request.target != resolution.cutex_session_id)
        || resolution.rename_notes.len() > 1
        || resolution.rename_notes.iter().any(|n| {
            n.cutex_session_id != resolution.cutex_session_id
                || n.current_name != resolution.current_name
                || n.scope != resolution.scope
                || n.requested_name != request.target
        })
    {
        return Err(Error::new("peer_protocol_error", 502));
    }
    Ok((
        peer,
        json!({"cutexSessionId":resolution.cutex_session_id,
        "name":resolution.current_name,"scope":resolution.scope,
        "indexRevision":resolution.index_revision,"renameNotes":resolution.rename_notes}),
    ))
}

/// Used only on an existing permissive tool-result detail surface. Neither
/// tell/create wire DTOs nor old receiver receipts gain new fields.
pub fn detail(target: &Value) -> String {
    serde_json::from_value::<Vec<crate::agent_names::RenameNote>>(target["renameNotes"].clone())
        .unwrap_or_default()
        .into_iter()
        .map(|note| note.text())
        .collect::<Vec<_>>()
        .join(" ")
}
