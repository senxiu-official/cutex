//! Typed owner endpoints for local Personal Assistants.
use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::management::{
    control_plane::HumanManagementPrincipal, server::ManagementRequestContext,
};
use crate::personal_assistants::{self as pa, host, Assistant, Config, Fault, Seat, Store, Update};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::net::TcpStream;

pub const PREFIX: &str = "/v2/personal-assistants";
pub(crate) const CALENDAR_IMPORT_MAX_BYTES: usize = 1024 * 1024;
pub fn is_path(path: &str) -> bool {
    path == PREFIX
        || path
            .strip_prefix(PREFIX)
            .is_some_and(|tail| tail.starts_with('/'))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Create {
    action_id: String,
    config: Config,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Edit {
    action_id: String,
    expected_revision: u64,
    change: Update,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Decision {
    action_id: String,
    expected_revision: u64,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Confirm {
    action_id: String,
    expected_revision: u64,
    expected_assistant_revision: u64,
    director: Seat,
}
fn decode<T: serde::de::DeserializeOwned>(request: &SimpleHttpRequest) -> anyhow::Result<T> {
    decode_limited(request, 64 * 1024)
}
fn decode_limited<T: serde::de::DeserializeOwned>(
    request: &SimpleHttpRequest,
    maximum: usize,
) -> anyhow::Result<T> {
    if request.body.len() > maximum {
        return Err(Fault("body_too_large").into());
    }
    serde_json::from_slice(&request.body).map_err(|_| Fault("invalid_arguments").into())
}
fn page(path: &str) -> anyhow::Result<(String, usize)> {
    let cursor = crate::http::query::query_value(path, "cursor").unwrap_or_default();
    let limit = crate::http::query::query_value(path, "limit")
        .map(|v| v.parse::<usize>())
        .transpose()
        .map_err(|_| Fault("invalid_limit"))?
        .unwrap_or(50);
    if cursor.len() > 160 || !(1..=100).contains(&limit) {
        return Err(Fault("invalid_page").into());
    }
    Ok((cursor, limit))
}
fn numeric(cursor: &str, default: i64) -> anyhow::Result<i64> {
    if cursor.is_empty() {
        return Ok(default);
    }
    cursor
        .parse::<i64>()
        .ok()
        .filter(|n| *n >= 0)
        .ok_or_else(|| Fault("invalid_cursor").into())
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ListItem {
    #[serde(flatten)]
    assistant: Assistant,
    availability: &'static str,
    active_profile: Option<String>,
}

fn list_item(row: Assistant) -> anyhow::Result<ListItem> {
    let record = row
        .session_id
        .as_deref()
        .map(crate::session::store::shared_session_row)
        .transpose()?
        .flatten();
    let availability = if !row.enabled {
        "disabled"
    } else if row.session_id.is_none() {
        "provisioning"
    } else {
        match record.as_deref() {
            None => "sessionMissing",
            Some(r) if !r.is_active() || r.retired_at.is_some() => "sessionInactive",
            Some(r) if host::runtime_supported(r) => "ready",
            Some(_) => "runtimeUnsupported",
        }
    };
    Ok(ListItem {
        assistant: row,
        availability,
        active_profile: record.and_then(|r| r.profile.clone()),
    })
}
fn view(store: &Store, row: Assistant) -> anyhow::Result<Value> {
    let mut overlaps = store.overlap(&row)?;
    let more = overlaps.len() > 100;
    overlaps.truncate(100);
    let item = list_item(row)?;
    Ok(
        json!({"assistant":item.assistant,"overlaps":overlaps,"moreOverlaps":more,"availability":item.availability,
        "activeProfile":item.active_profile,
        "capabilities":pa::capabilities()}),
    )
}

fn request_list(
    store: &Store,
    assistant: Option<&str>,
    path: &str,
    cursor: &str,
    limit: usize,
) -> anyhow::Result<Value> {
    let query = |key| crate::http::query::query_value(path, key);
    let project = match (query("instanceId"), query("projectId")) {
        (Some(instance_id), Some(project_id)) => Some(pa::ProjectRef {
            instance_id,
            project_id,
        }),
        (None, None) => None,
        _ => return Err(Fault("invalid_project_ref").into()),
    };
    let state = query("state")
        .map(|v| serde_json::from_value(json!(v)))
        .transpose()
        .map_err(|_| Fault("invalid_request_state"))?;
    let epoch = query("directorEpoch")
        .map(|v| v.parse::<i64>())
        .transpose()
        .map_err(|_| Fault("invalid_director_seat"))?;
    let filter = pa::owner_views::RequestFilter {
        assistant,
        project,
        director: query("directorSessionId"),
        epoch,
        state,
    };
    store.owner_requests(filter, numeric(cursor, i64::MAX)?, limit)
}
fn dispatch(
    request: &SimpleHttpRequest,
    principal: &HumanManagementPrincipal,
    context: &ManagementRequestContext,
) -> anyhow::Result<(u16, Value)> {
    let clean = request.path.split('?').next().unwrap_or("");
    let tail = clean.strip_prefix(PREFIX).ok_or(Fault("route_not_found"))?;
    let parts: Vec<_> = tail
        .strip_prefix('/')
        .map(|s| s.split('/').collect())
        .unwrap_or_default();
    let (cursor, limit) = page(&request.path)?;
    let store = Store::open_default()?;
    let actor = "owner";
    match (request.method.as_str(), parts.as_slice()) {
        ("GET", []) => {
            let items = store.list(&cursor, limit)?;
            let next = items.last().map(|r| r.id.clone());
            let items = items
                .into_iter()
                .map(list_item)
                .collect::<anyhow::Result<Vec<_>>>()?;
            Ok((
                200,
                json!({"items":items,"nextCursor":next,"homeInstanceId":crate::management::instance::instance_id()}),
            ))
        }
        ("POST", []) => {
            let a: Create = decode(request)?;
            let row = host::with_directory(|dir| {
                store.begin(actor, &a.action_id, a.config, |cfg| {
                    host::validate_scope(cfg, dir)?;
                    // Avoid reserving an unusable binding for a mistyped profile.
                    // The normal creator resolves a provider selector once, at creation.
                    use super::profile_auto_pick::{requested, Requested};
                    match requested(&cfg.profile) {
                        Requested::Profile => {
                            crate::config::new_session::resolve_profile_name(&cfg.profile)
                                .map_err(|_| Fault("profile_unavailable"))?;
                            Ok(())
                        }
                        Requested::Provider(family) if !family.is_empty() => Ok(()),
                        _ => Err(Fault("invalid_profile").into()),
                    }
                })
            })?;
            match host::provision(&store, &row.id, principal, context) {
                Ok(ready) => Ok((201, view(&store, ready)?)),
                Err(e)
                    if e.downcast_ref::<Fault>()
                        .is_some_and(|e| e.0 == "provisioning_in_progress") =>
                {
                    Ok((202, view(&store, store.get(&row.id)?)?))
                }
                Err(e) => Err(e),
            }
        }
        ("GET", ["requests"]) => Ok((
            200,
            request_list(&store, None, &request.path, &cursor, limit)?,
        )),
        ("GET", [id]) => Ok((200, view(&store, store.get(id)?)?)),
        ("PATCH", [id]) => {
            let a: Edit = decode(request)?;
            let row = if matches!(
                a.change,
                Update::Settings { .. } | Update::ScopedSettings { .. }
            ) {
                host::with_directory(|dir| {
                    store.configure(
                        actor,
                        id,
                        &a.action_id,
                        a.expected_revision,
                        &a.change,
                        |cfg| host::validate_scope(cfg, dir),
                    )
                })?
            } else {
                store.configure(
                    actor,
                    id,
                    &a.action_id,
                    a.expected_revision,
                    &a.change,
                    |_| Ok(()),
                )?
            };
            Ok((200, view(&store, row)?))
        }
        ("GET", [id, "agenda"]) => {
            let row = store.get(id)?;
            let day = crate::http::query::query_value(&request.path, "date");
            let reminders = crate::http::query::query_value(&request.path, "reminderCursor");
            let schedules = crate::http::query::query_value(&request.path, "scheduleCursor");
            let project = crate::http::query::query_value(&request.path, "projectCursor");
            let due = crate::http::query::query_value(&request.path, "dueCursor");
            let calendar = crate::http::query::query_value(&request.path, "calendarCursor");
            Ok((
                200,
                pa::agenda::for_day(
                    &store,
                    &row,
                    day.as_deref(),
                    numeric(&cursor, 0)?,
                    pa::agenda::Cursors {
                        reminder: reminders.as_deref(),
                        schedule: schedules.as_deref(),
                        project: project.as_deref(),
                        due: due.as_deref(),
                        calendar: calendar.as_deref(),
                    },
                    limit,
                )?,
            ))
        }
        ("POST", [id, "templates"]) => Ok((
            201,
            json!({"schedule":pa::recurring::confirm(&store,id,None,decode(request)?)?}),
        )),
        ("PUT", [id, "templates", key]) => Ok((
            200,
            json!({"schedule":pa::recurring::confirm(&store,id,Some(key),decode(request)?)?}),
        )),
        ("GET", [id, "templates"]) => {
            store.get(id)?;
            let page = crate::schedules::Store::open_default()?.templates(id, &cursor, limit)?;
            Ok((
                200,
                json!({"items":page.data,"nextCursor":page.next_cursor}),
            ))
        }
        ("GET", [id, "templates", key]) => Ok((
            200,
            json!({"schedule":crate::schedules::Store::open_default()?.template(id,key)?}),
        )),
        ("POST", [id, "templates", key, action @ ("pause" | "cancel")]) => {
            let result = crate::schedules::Store::open_default()?.stop(
                key,
                decode(request)?,
                chrono::Utc::now(),
                Some(id),
                *action == "cancel",
            )?;
            Ok((200, json!({"schedule":result})))
        }
        ("GET", [id, "templates", key, "runs"]) => {
            let schedules = crate::schedules::Store::open_default()?;
            schedules.template(id, key)?;
            let before = if cursor.is_empty() {
                None
            } else {
                Some(numeric(&cursor, 0)? as u64)
            };
            let mut page = schedules.runs(key, before, limit)?;
            for run in &mut page.data {
                run.lease_token = None;
            }
            Ok((
                200,
                json!({"items":page.data,"nextCursor":page.next_cursor}),
            ))
        }
        ("GET", [id, "templates", key, "audit"]) => {
            let events = crate::schedules::Store::open_default()?.template_history(
                id,
                key,
                numeric(&cursor, i64::MAX)?,
                limit,
            )?;
            let next = events.last().and_then(|r| r["seq"].as_i64());
            Ok((200, json!({"items":events,"nextCursor":next})))
        }
        ("GET", [id, "calendar"]) => Ok((200, store.calendar_info(id)?)),
        ("PUT", [id, "calendar"]) => {
            let a: pa::calendar::Import = decode_limited(request, CALENDAR_IMPORT_MAX_BYTES)?;
            Ok((
                200,
                store.replace_calendar(id, &a.action_id, a.expected_revision, Some(&a.snapshot))?,
            ))
        }
        ("DELETE", [id, "calendar"]) => {
            let a: Decision = decode(request)?;
            Ok((
                200,
                store.replace_calendar(id, &a.action_id, a.expected_revision, None)?,
            ))
        }
        ("GET", [id, "audit"]) => {
            store.get(id)?;
            let rows = store.audit(id, numeric(&cursor, i64::MAX)?, limit)?;
            let next = rows.last().and_then(|r| r["seq"].as_i64());
            Ok((200, json!({"items":rows,"nextCursor":next})))
        }
        ("GET", [id, "notes"]) => {
            let query = crate::http::query::query_value(&request.path, "q").unwrap_or_default();
            Ok((200, store.owner_notes(id, &query, &cursor, limit)?))
        }
        ("GET", [id, "requests"]) => Ok((
            200,
            request_list(&store, Some(id), &request.path, &cursor, limit)?,
        )),
        ("GET", [id, "requests", request_id]) => {
            let r = store.request(request_id)?;
            if r.assistant_id != *id {
                return Err(Fault("request_not_found").into());
            }
            let current = host::with_directory(|dir| host::seat(dir, &r.project_id));
            let (seat, issue) = match current {
                Ok(s) => (Some(s), None),
                Err(e) if e.is::<Fault>() => (None, Some(e.to_string())),
                Err(e) => return Err(e),
            };
            let row = store.get(id)?;
            let seat_changed = seat.as_ref() != Some(&r.director);
            let stale = match r.state {
                pa::RequestState::AwaitingOwner | pa::RequestState::Queued => {
                    seat_changed
                        || !row.enabled
                        || !row.config.project_ids.contains(&r.project_id)
                        || !row.request_revision_current(&r)
                }
                pa::RequestState::Accepted | pa::RequestState::Clarification => seat_changed,
                _ => false,
            };
            Ok((
                200,
                json!({"request":r,"currentDirector":seat,"currentAssistantRevision":row.revision,"projectIssue":issue,"needsReview":stale}),
            ))
        }
        ("POST", [id, "requests", request_id, "confirm"]) => {
            let a: Confirm = decode(request)?;
            let r = store.request(request_id)?;
            if r.assistant_id != *id {
                return Err(Fault("request_not_found").into());
            }
            let confirmed = host::with_directory(|dir| {
                let current = host::seat(dir, &r.project_id)?;
                store.confirm(
                    actor,
                    id,
                    request_id,
                    &a.action_id,
                    a.expected_revision,
                    a.expected_assistant_revision,
                    &a.director,
                    &current,
                )
            })?;
            Ok((200, json!({"request":confirmed})))
        }
        ("POST", [id, "requests", request_id, "cancel"]) => {
            let a: Decision = decode(request)?;
            Ok((
                200,
                json!({"request":store.cancel(actor,id,request_id,&a.action_id,a.expected_revision)?}),
            ))
        }
        _ => Err(Fault("route_not_found").into()),
    }
}
pub fn handle(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    principal: &HumanManagementPrincipal,
    context: &ManagementRequestContext,
) -> anyhow::Result<()> {
    match dispatch(request, principal, context) {
        Ok((status, body)) => write_json_response(
            stream,
            status,
            match status {
                201 => "Created",
                202 => "Accepted",
                _ => "OK",
            },
            &body,
        ),
        Err(e) => {
            let (status, code) = pa::error_status(&e);
            super::server::write_v2_error(
                stream,
                status,
                match status {
                    400 => "Bad Request",
                    403 => "Forbidden",
                    404 => "Not Found",
                    409 => "Conflict",
                    _ => "Service Unavailable",
                },
                code,
                pa::error_message(code),
                status == 503,
                json!({}),
            )
        }
    }
}
