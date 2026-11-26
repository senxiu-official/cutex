use super::{
    host,
    inbox::{Draft, Response},
    *,
};
use anyhow::Result;
use serde_json::{json, Map, Value};

pub const PATH: &str = "/api/mcp/v1/personal-assistants";
pub const ASSISTANT: &[&str] = &[
    "pa_self",
    "pa_project_read",
    "pa_request_prepare",
    "pa_request_revise",
    "pa_request_list",
    "pa_request_read",
    "pa_memory_search",
    "pa_memory_write",
    "pa_agenda_read",
    "pa_reminder_set",
    "pa_reminder_cancel",
];
pub const DIRECTOR: &[&str] = &["director_request_list", "director_request_respond"];
pub fn is_tool(n: &str) -> bool {
    ASSISTANT.contains(&n) || DIRECTOR.contains(&n)
}

struct Spec {
    name: &'static str,
    description: &'static str,
    required: &'static [&'static str],
    optional: &'static [&'static str],
}
const SPECS: &[Spec] = &[
    Spec {
        name: "pa_self",
        description: "Read your Personal Assistant identity, current configuration and grants.",
        required: &[],
        optional: &[],
    },
    Spec {
        name: "pa_project_read",
        description: "Read a selected local project and its current Director seat.",
        required: &[],
        optional: &["projectRef","projectId"],
    },
    Spec {
        name: "pa_request_prepare",
        description: "Prepare a concrete Director request for the owner to review in the assistant conversation.",
        required: &["actionId", "outcome", "sourceRef"],
        optional: &["projectRef","projectId","constraints","dueAt"],
    },
    Spec {
        name: "pa_request_revise",
        description: "Revise a clarification or a request whose seat or grant changed. The owner reviews the new version.",
        required: &["actionId", "requestId", "expectedRevision", "outcome", "sourceRef"],
        optional: &["projectRef","projectId","constraints","dueAt"],
    },
    Spec {
        name: "pa_request_list",
        description: "Page through your requests in creation order.",
        required: &[],
        optional: &["after", "limit"],
    },
    Spec {
        name: "pa_request_read",
        description: "Read the latest state and Director reply for one of your requests.",
        required: &["requestId"],
        optional: &[],
    },
    Spec {
        name: "pa_memory_search",
        description: "Search your own coordination notes, ordered by key.",
        required: &[],
        optional: &["query", "afterKey", "limit"],
    },
    Spec {
        name: "pa_memory_write",
        description: "Save a sourced coordination note. Use revision zero for a new key.",
        required: &["actionId", "key", "expectedRevision", "content", "sourceRef"],
        optional: &[],
    },
    Spec {
        name: "pa_agenda_read",
        description: "Read open requests and reminders and schedules due on a home-local date (YYYY-MM-DD, today by default).",
        required: &[],
        optional: &["after", "limit", "date", "reminderCursor", "scheduleCursor","projectCursor","dueCursor","calendarCursor"],
    },
    Spec {
        name: "pa_reminder_set",
        description: "Set a reminder addressed to your own assistant session at an RFC 3339 time.",
        required: &["alarmId", "at", "message"],
        optional: &[],
    },
    Spec {
        name: "pa_reminder_cancel",
        description: "Cancel one of your own reminders.",
        required: &["alarmId"],
        optional: &[],
    },
    Spec {
        name: "director_request_list",
        description: "Read requests for your current Director seat in admission order. clarifyUnavailable explains when a queued or accepted request cannot enter clarification.",
        required: &[],
        optional: &["after", "limit"],
    },
    Spec {
        name: "director_request_respond",
        description: "Accept, clarify, decline, update progress or report completion on a Personal Assistant request.",
        required: &["actionId", "requestId", "expectedRevision", "response"],
        optional: &["detail"],
    },
];
fn field(key: &str) -> Value {
    match key {
        "projectRef" => {
            json!({"type":"object","properties":{"instanceId":{"type":"string"},"projectId":{"type":"string"}},"required":["instanceId","projectId"],"additionalProperties":false})
        }
        "after" | "expectedRevision" => json!({"type":"integer","minimum":0}),
        "limit" => json!({"type":"integer","minimum":1,"maximum":100}),
        "response" => {
            json!({"type":"string","enum":["accept","clarify","decline","progress","report"]})
        }
        _ => json!({"type":"string"}),
    }
}
pub fn definitions() -> Vec<Value> {
    SPECS.iter().map(|spec|{
        let properties:Map<_,_>=spec.required.iter().chain(spec.optional).map(|&key|(key.into(),field(key))).collect();
        let mut schema=json!({"type":"object","properties":properties,"required":spec.required,"additionalProperties":false});
        if spec.optional.contains(&"projectRef") {
            schema["anyOf"]=json!([{"required":["projectRef"]},{"required":["projectId"]}]);
        }
        json!({"name":spec.name,"description":spec.description,"inputSchema":schema})
    }).collect()
}
fn string<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args[key].as_str().ok_or_else(|| fault("invalid_arguments"))
}
fn revision(args: &Value) -> Result<u64> {
    args["expectedRevision"]
        .as_u64()
        .ok_or_else(|| fault("invalid_arguments"))
}
fn limit(args: &Value) -> Result<usize> {
    let n = args
        .get("limit")
        .map(|v| v.as_u64().ok_or_else(|| fault("invalid_limit")))
        .transpose()?
        .unwrap_or(50);
    require((1..=100).contains(&n), "invalid_limit")?;
    Ok(n as usize)
}
fn after(args: &Value) -> Result<i64> {
    let n = args
        .get("after")
        .map(|v| v.as_i64().ok_or_else(|| fault("invalid_cursor")))
        .transpose()?
        .unwrap_or(0);
    require(n >= 0, "invalid_cursor")?;
    Ok(n)
}
fn scoped_project(row: &Assistant, args: &Value) -> Result<ProjectRef> {
    let legacy = args
        .get("projectId")
        .map(|v| v.as_str().ok_or_else(|| fault("invalid_arguments")))
        .transpose()?;
    let reference = match args.get("projectRef") {
        Some(value) => serde_json::from_value::<ProjectRef>(value.clone())
            .map_err(|_| fault("invalid_project_ref"))?,
        None => row
            .config
            .project_ref(legacy.ok_or_else(|| fault("invalid_project_ref"))?),
    };
    reference.local_project()?;
    require(
        reference.instance_id == row.config.home_instance_id,
        "destination_scope_required",
    )?;
    require(
        legacy.is_none_or(|id| id == reference.project_id),
        "project_mismatch",
    )?;
    row.scoped(&reference.project_id)?;
    Ok(reference)
}
fn draft(args: &Value, project_id: String) -> Result<Draft> {
    Ok(Draft {
        project_id,
        outcome: string(args, "outcome")?.into(),
        constraints: args["constraints"].as_str().unwrap_or("").into(),
        source_ref: string(args, "sourceRef")?.into(),
        due_at: args
            .get("dueAt")
            .map(|v| serde_json::from_value(v.clone()).map_err(|_| fault("invalid_date")))
            .transpose()?,
    })
}
pub fn call_body(caller: &str, body: &[u8]) -> Result<Value> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Call {
        tool: String,
        arguments: Value,
    }
    require(body.len() <= 64 * 1024, "body_too_large")?;
    let request: Call = serde_json::from_slice(body).map_err(|_| fault("invalid_arguments"))?;
    call(
        &Store::open_default()?,
        caller,
        &request.tool,
        &request.arguments,
    )
}

fn director_item(row: inbox::DirectorInboxItem) -> Result<Value> {
    let mut item = serde_json::to_value(row.request)?;
    if row.clarification_full {
        item["clarifyUnavailable"] =
            json!({"code":"open_request_limit","message":error_message("open_request_limit")});
    }
    Ok(item)
}

pub fn call(store: &Store, caller: &str, name: &str, args: &Value) -> Result<Value> {
    let spec = SPECS
        .iter()
        .find(|s| s.name == name)
        .ok_or_else(|| fault("unknown_tool"))?;
    let fields = args.as_object().ok_or_else(|| fault("invalid_arguments"))?;
    require(
        spec.required.iter().all(|key| fields.contains_key(*key))
            && fields.keys().all(|key| {
                spec.required.contains(&key.as_str()) || spec.optional.contains(&key.as_str())
            }),
        "invalid_arguments",
    )?;
    for (key, value) in fields {
        let valid = match key.as_str() {
            "after" | "limit" | "expectedRevision" => value.is_u64(),
            "projectRef" => value.is_object(),
            _ => value.is_string(),
        };
        require(valid, "invalid_arguments")?;
    }
    if DIRECTOR.contains(&name) {
        match store.for_session(caller) {
            Ok(_) => return Err(fault("director_required")),
            Err(e) if matches!(e.downcast_ref::<Fault>(), Some(Fault("assistant_required"))) => {}
            Err(e) => return Err(e),
        }
        return host::with_directory(|dir| {
            require(
                dir.projects
                    .values()
                    .any(|p| p.authorized_director_session.as_str() == caller),
                "director_required",
            )?;
            if name == "director_request_list" {
                let (rows, cursor) =
                    store.director_inbox(caller, after(args)?, limit(args)?, |r| {
                        host::seat(dir, &r.project_id).is_ok_and(|seat| seat == r.director)
                    })?;
                let visible = rows
                    .into_iter()
                    .map(director_item)
                    .collect::<Result<Vec<_>>>()?;
                return Ok(json!({"items":visible,"nextCursor":cursor}));
            }
            let request = store.request(string(args, "requestId")?)?;
            require(
                request.director.session_id.as_deref() == Some(caller),
                "request_not_found",
            )?;
            let seat = host::seat(dir, &request.project_id)?;
            let response: Response = serde_json::from_value(args["response"].clone())
                .map_err(|_| fault("invalid_arguments"))?;
            Ok(json!(store.respond(
                caller,
                &request.id,
                string(args, "actionId")?,
                revision(args)?,
                response,
                args["detail"].as_str().unwrap_or(""),
                &seat
            )?))
        });
    }
    let row = store.for_session(caller)?;
    row.require_active()?;
    match name {
        "pa_self" => {
            let mut result = json!(row);
            result["capabilities"] = capabilities();
            Ok(result)
        }
        "pa_project_read" => {
            let reference = scoped_project(&row, args)?;
            let mut result =
                host::with_directory(|dir| host::project_view(dir, &reference.project_id))?;
            result["projectRef"] = json!(reference);
            Ok(result)
        }
        "pa_request_prepare" | "pa_request_revise" => {
            let draft = draft(args, scoped_project(&row, args)?.project_id)?;
            host::with_directory(|dir| {
                let seat = host::seat(dir, &draft.project_id)?;
                if name == "pa_request_prepare" {
                    Ok(json!(store.prepare(
                        caller,
                        string(args, "actionId")?,
                        &draft,
                        &seat
                    )?))
                } else {
                    Ok(json!(store.revise(
                        caller,
                        string(args, "requestId")?,
                        string(args, "actionId")?,
                        revision(args)?,
                        &draft,
                        &seat
                    )?))
                }
            })
        }
        "pa_request_read" => {
            let r = store.request(string(args, "requestId")?)?;
            require(r.assistant_id == row.id, "request_not_found")?;
            row.scoped(&r.project_id)?;
            Ok(json!(r))
        }
        "pa_agenda_read" => super::agenda::for_day(
            store,
            &row,
            args["date"].as_str(),
            after(args)?,
            super::agenda::Cursors {
                reminder: args["reminderCursor"].as_str(),
                schedule: args["scheduleCursor"].as_str(),
                project: args["projectCursor"].as_str(),
                due: args["dueCursor"].as_str(),
                calendar: args["calendarCursor"].as_str(),
            },
            limit(args)?,
        ),
        "pa_request_list" => {
            let rows = store.requests(Some(&row.id), None, after(args)?, limit(args)?)?;
            let cursor = rows.last().map(|(seq, _)| *seq);
            let items: Vec<_> = rows
                .into_iter()
                .map(|(_, r)| r)
                .filter(|r| row.config.project_ids.contains(&r.project_id))
                .collect();
            Ok(json!({"items":items,"nextCursor":cursor}))
        }
        "pa_memory_search" => Ok(
            json!({"items":store.memory(caller,args["query"].as_str().unwrap_or(""),args["afterKey"].as_str().unwrap_or(""),limit(args)?)?}),
        ),
        "pa_memory_write" => store.memory_write(
            caller,
            string(args, "actionId")?,
            string(args, "key")?,
            revision(args)?,
            string(args, "content")?,
            string(args, "sourceRef")?,
        ),
        "pa_reminder_set" => store.while_active(caller, || {
            crate::agent_bus::alarms::set(
                caller,
                crate::agent_bus::alarms::SetArgs {
                    alarm_id: string(args, "alarmId")?.into(),
                    to: caller.into(),
                    message: string(args, "message")?.into(),
                    at: Some(string(args, "at")?.into()),
                    delay_seconds: None,
                },
            )
        }),
        "pa_reminder_cancel" => store.while_active(caller, || {
            crate::agent_bus::alarms::cancel(Some(caller), string(args, "alarmId")?)
        }),
        _ => Err(fault("unknown_tool")),
    }
}
