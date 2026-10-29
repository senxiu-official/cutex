//! `service_*` MCP tools: agents register the web services they start.
//! Caller identity comes from the authenticated runtime (Agent Bus), never
//! from arguments; the Bus serves `POST /api/services/v1`.

use serde::Deserialize;
use serde_json::json;
use serde_json::Map;
use serde_json::Value;

pub(super) const REGISTER: &str = "service_register";
pub(super) const UPDATE: &str = "service_update";
pub(super) const RESTART: &str = "service_restart";
pub(super) const UNREGISTER: &str = "service_unregister";
pub(super) const LIST: &str = "service_list";
pub(super) const PATH: &str = "/api/services/v1";

pub(super) fn is_tool(name: &str) -> bool {
    matches!(name, REGISTER | UPDATE | RESTART | UNREGISTER | LIST)
}

fn tile_schema(nullable: bool) -> Value {
    json!({"type": if nullable { json!(["object","null"]) } else { json!("object") },"description":"Home page tile (all fields optional): title ≤60, subtitle ≤120, lines ≤3×≤120, badge ≤12, image (PNG/JPEG/WebP path), progress {value 0–1, label ≤40}, actions ≤3×{label ≤20, path} (opened in the app), refreshSeconds ≥60, sizes [s,m,w,l]. Paths are inside this service and start with '/' (no URLs); unknown fields are dropped."})
}

fn tile_path_schema(nullable: bool) -> Value {
    json!({"type": if nullable { json!(["string","null"]) } else { json!("string") },"description":"Path in this service that serves a live tile as JSON (same fields, ≤16 KiB), polled by cutex. Default /.well-known/cutex-tile.json when `tile` is not set."})
}

fn scope_schema(update: bool) -> Value {
    let description = if update {
        "Set project to turn a service you manage (of your current project) into a project service; only the owner turns it back into session."
    } else {
        "Who manages it besides the owner. session: only you. project: also your project's current Director (whoever holds the seat at the time; it sees the command, cwd and env keys) and you while you are in the project; it is not reclaimed when you are archived. Default: project when you are the current Director of one project, otherwise session; project needs a current project (project_scope_unavailable). Re-registering keeps the existing scope."
    };
    json!({"type":"string","enum":["session","project"],"description":description})
}

fn command_schema() -> Value {
    json!({"description":"For runtime managed: a shell command line, or an argv array. `{port}` is replaced by the service port and PORT is set; the server must listen on that port.","anyOf":[{"type":"string"},{"type":"array","items":{"type":"string"}}]})
}

pub(super) fn tools() -> Vec<Value> {
    vec![
        json!({"name":REGISTER,"description":"Register a web server or static HTML report (dev server, preview, API, docs) so the owner can find, open and monitor it. Do this instead of picking a random port: omit `port` and cutex allocates a free one and returns it, then start your server on that port, bound to 127.0.0.1 (managed commands get PORT and HOST=127.0.0.1). With runtime `external` (default) you run the process and cutex only health-checks it; with `managed` cutex runs `command`, restarts it on crashes, and starts it again when the owner opens it after a stop. To show the owner a single page, chart or picture, publish it with artifact_publish show: true instead (it opens beside your conversation); services are for real servers and multi-file sites. A static service serves an absolute root (directory or .html file, no port/command/env): a root inside your cwd or project workspace needs nothing; any other existing root outside the cutex home needs the owner's approval (approval_required, one approval together with a persistent lifetime); the cutex home (except your own cutex-made scratch cwd), filesystem roots and the user home itself are always refused. The request is fully validated before any approval is asked, so fix static_root_forbidden / invalid errors first. Vite commands must use --port {port} --strictPort. New services default to ephemeral and are reclaimed after idle/TTL or owner archive; `idle_ttl_minutes` may request a longer idle window up to the host's maximum. Persistent may need owner approval and may suggest `purpose` foreground/background; the owner can reorganize it later. Link the returned cutex:service/ link. reveal requests an owner notification. Re-registering your own slug updates it while preserving owner organization. Unregister it when the service is gone for good. `tile` / `tile_path` give it a tile. A `managed` command runs outside your sandbox, so unless you already run with full access and approval `never` it needs the owner's approval: the call waits up to 2 minutes for the owner's decision and then proceeds (or answers approval_denied); if the owner has not decided by then it answers `approval_required` with an approval_id: retry the same call after the owner approves (end your turn or set an alarm instead of polling).","inputSchema":{"type":"object","properties":{
            "slug":{"type":"string","description":"Stable id, unique on this host: a-z, 0-9 and '-', e.g. `myapp-web`."},
            "display_name":{"type":"string","description":"Human name; defaults to the slug."},
            "reveal":{"type":"boolean","description":"Request that the owner see this page (rate limited)."},
            "port":{"type":"integer","minimum":1,"maximum":65535,"description":"Omit to get a free port allocated."},
            "path":{"type":"string","description":"URL path to open and health-check, e.g. `/` or `/health`."},
            "runtime":{"type":"string","enum":["external","managed","static"],"description":"Default external."},
            "root":{"type":"string","description":"Absolute static directory or .html file. Inside your cwd or project workspace: no approval; elsewhere (outside the cutex home, not a filesystem root or the user home): owner approval."},
            "lifetime":{"type":"string","enum":["ephemeral","persistent"]},
            "purpose":{"type":"string","enum":["foreground","background"],"description":"Optional hint for a persistent service only. Foreground is something the owner opens or uses; background is infrastructure."},
            "idle_ttl_minutes":{"type":"integer","minimum":5,"description":"Ephemeral idle TTL to request, in minutes. The host refuses values above its configured maximum."},
            "command":command_schema(),
            "cwd":{"type":"string","description":"Absolute working directory for managed."},
            "env":{"type":"object","additionalProperties":{"type":"string"},"description":"Extra environment for managed (values are never shown back)."},
            "tile":tile_schema(false),
            "tile_path":tile_path_schema(false),
            "scope":scope_schema(false)
        },"required":["slug"],"additionalProperties":false}}),
        json!({"name":UPDATE,"description":"Change a service you manage (port, path, name, command, cwd, env, lifetime, idle TTL, tile, scope): your own, or a project service of the project you direct or registered while in it (service_list youMayManage). Upgrading lifetime to persistent may require owner approval (owner setting); approvals follow your own permissions. `path`/`cwd`/`command`/`tile`/`tile_path` null clears; env entries merge, null removes a key. Changing what a managed service runs (runtime, command, cwd, env) needs the owner's approval like registering it; restating unchanged values does not. To restart it unchanged (e.g. after editing its code) use service_restart.","inputSchema":{"type":"object","properties":{
            "slug":{"type":"string"},
            "display_name":{"type":"string"},
            "port":{"type":"integer","minimum":1,"maximum":65535},
            "path":{"type":["string","null"]},
            "runtime":{"type":"string","enum":["external","managed","static"]},
            "root":{"type":"string","description":"Absolute static directory or .html file. Inside your cwd or project workspace: no approval; elsewhere (outside the cutex home, not a filesystem root or the user home): owner approval."},
            "lifetime":{"type":"string","enum":["ephemeral","persistent"]},
            "idle_ttl_minutes":{"type":"integer","minimum":5,"description":"Ephemeral idle TTL to request, in minutes. The host refuses values above its configured maximum."},
            "command":command_schema(),
            "cwd":{"type":["string","null"]},
            "env":{"type":"object","additionalProperties":{"type":["string","null"]}},
            "tile":tile_schema(true),
            "tile_path":tile_path_schema(true),
            "scope":scope_schema(true)
        },"required":["slug"],"additionalProperties":false}}),
        json!({"name":RESTART,"description":"Restart a managed service you registered (e.g. after editing its code) with its unchanged command, cwd and env. No owner approval: the approval that let the command run covers its restarts. The call returns once the restart is requested; cutex stops the running process and starts it again about a second later (check service_list for health and process.startedAt). Refused for external and static services (not_managed: restart your own process) and for a service the owner stopped (service_stopped: only the owner starts it again). To change what it runs, use service_update instead (it restarts the service itself). Only services you manage (service_list youMayManage).","inputSchema":{"type":"object","properties":{"slug":{"type":"string"}},"required":["slug"],"additionalProperties":false}}),
        json!({"name":UNREGISTER,"description":"Remove a service you manage (managed ones are stopped): your own, or a project service you manage (service_list youMayManage). Removing a persistent project service another agent registered needs the owner's confirmation (waits like other approvals, else approval_required).","inputSchema":{"type":"object","properties":{"slug":{"type":"string"}},"required":["slug"],"additionalProperties":false}}),
        json!({"name":LIST,"description":"List web services you may use, with lifetime, cutex:service/ link, port, URL and health (reachable/unreachable/starting/stopped): your own, your project's (outside a project: those of the cutex agents you created and the ones they created; native subagents are not cutex agents) and the owner's shared ones. Each has ownerScope (session: managed by its registrant; project: also by the project's current Director) and youMayManage (whether you may update, restart or unregister it). Command, cwd and env keys are shown only for services you may manage. Check it before starting a server to reuse an existing one.","inputSchema":{"type":"object","properties":{"mine":{"type":"boolean","description":"Only services you registered."}},"required":[],"additionalProperties":false}}),
    ]
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolArgs {
    #[serde(default)]
    slug: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    port: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    path: Option<Value>,
    #[serde(default)]
    runtime: Option<String>,
    #[serde(default)]
    root: Option<String>,
    #[serde(default)]
    lifetime: Option<String>,
    #[serde(default)]
    purpose: Option<String>,
    #[serde(default)]
    idle_ttl_minutes: Option<u32>,
    #[serde(default)]
    reveal: Option<bool>,
    #[serde(default, deserialize_with = "present")]
    command: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    cwd: Option<Value>,
    #[serde(default)]
    env: Option<Value>,
    #[serde(default)]
    mine: Option<bool>,
    #[serde(default, deserialize_with = "present")]
    tile: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    tile_path: Option<Value>,
    #[serde(default)]
    scope: Option<String>,
}

/// Keeps an explicit `null` as `Some(Value::Null)`.
fn present<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

/// Translate tool arguments into the Bus request body.
pub(super) fn request_body(name: &str, args: Value) -> anyhow::Result<Value> {
    let args = if args.is_null() { json!({}) } else { args };
    let parsed: ToolArgs = serde_json::from_value(args)?;
    let op = match name {
        REGISTER => "register",
        UPDATE => "update",
        RESTART => "restart",
        UNREGISTER => "unregister",
        LIST => "list",
        _ => anyhow::bail!("unsupported services tool"),
    };
    let mut out = Map::new();
    if op == "list" {
        anyhow::ensure!(
            parsed.slug.is_none()
                && parsed.display_name.is_none()
                && parsed.port.is_none()
                && parsed.path.is_none()
                && parsed.runtime.is_none()
                && parsed.command.is_none()
                && parsed.cwd.is_none()
                && parsed.env.is_none()
                && parsed.root.is_none()
                && parsed.lifetime.is_none()
                && parsed.purpose.is_none()
                && parsed.idle_ttl_minutes.is_none()
                && parsed.reveal.is_none()
                && parsed.tile.is_none()
                && parsed.tile_path.is_none()
                && parsed.scope.is_none(),
            "list takes only `mine`"
        );
        if let Some(mine) = parsed.mine {
            out.insert("mine".into(), json!(mine));
        }
        return Ok(json!({"op": op, "args": out}));
    }
    anyhow::ensure!(parsed.mine.is_none(), "`mine` is only for list");
    let slug = parsed
        .slug
        .filter(|slug| !slug.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("slug is required"))?;
    out.insert("slug".into(), json!(slug));
    if op == "unregister" || op == "restart" {
        anyhow::ensure!(
            parsed.display_name.is_none()
                && parsed.port.is_none()
                && parsed.path.is_none()
                && parsed.runtime.is_none()
                && parsed.command.is_none()
                && parsed.cwd.is_none()
                && parsed.env.is_none()
                && parsed.root.is_none()
                && parsed.lifetime.is_none()
                && parsed.purpose.is_none()
                && parsed.idle_ttl_minutes.is_none()
                && parsed.reveal.is_none()
                && parsed.tile.is_none()
                && parsed.tile_path.is_none()
                && parsed.scope.is_none(),
            "{op} takes only `slug`"
        );
        return Ok(json!({"op": op, "args": out}));
    }
    if let Some(value) = parsed.display_name {
        out.insert("displayName".into(), json!(value));
    }
    if let Some(value) = parsed.port {
        out.insert("port".into(), value);
    }
    if let Some(value) = parsed.root {
        out.insert("root".into(), json!(value));
    }
    if let Some(value) = parsed.lifetime {
        out.insert("lifetime".into(), json!(value));
    }
    if let Some(value) = parsed.purpose {
        anyhow::ensure!(op == "register", "purpose is only for register");
        out.insert("purpose".into(), json!(value));
    }
    if let Some(value) = parsed.idle_ttl_minutes {
        out.insert("idleTtlMinutes".into(), json!(value));
    }
    if let Some(value) = parsed.reveal {
        anyhow::ensure!(op == "register", "reveal is only for register");
        out.insert("reveal".into(), json!(value));
    }
    if let Some(value) = parsed.runtime {
        out.insert("runtime".into(), json!(value));
    }
    if let Some(value) = parsed.scope {
        anyhow::ensure!(
            matches!(value.as_str(), "session" | "project"),
            "scope must be session or project"
        );
        out.insert("ownerScope".into(), json!(value));
    }
    for (key, value) in [
        ("path", parsed.path),
        ("command", parsed.command),
        ("cwd", parsed.cwd),
        ("tile", parsed.tile),
        ("tilePath", parsed.tile_path),
    ] {
        if let Some(value) = value {
            anyhow::ensure!(
                op == "update" || !value.is_null(),
                "`{key}` must not be null"
            );
            out.insert(key.into(), value);
        }
    }
    if let Some(value) = parsed.env {
        out.insert("env".into(), value);
    }
    Ok(json!({"op": op, "args": out}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_schemas_are_closed_and_take_no_caller_authority() {
        for tool in tools() {
            assert_eq!(tool["inputSchema"]["additionalProperties"], false);
            for key in [
                "owner",
                "owner_cutex_session_id",
                "project_id",
                "caller",
                "action_id",
                "revision",
            ] {
                assert!(tool["inputSchema"]["properties"].get(key).is_none());
            }
            assert!(is_tool(tool["name"].as_str().unwrap()));
        }
    }

    #[test]
    fn arguments_translate_to_bus_requests() {
        let body = request_body(
            REGISTER,
            json!({"slug":"web","display_name":"Web","runtime":"managed","command":["node","s.js"],"env":{"A":"1"}}),
        )
        .unwrap();
        assert_eq!(body["op"], "register");
        assert_eq!(body["args"]["displayName"], "Web");
        assert_eq!(body["args"]["runtime"], "managed");
        assert_eq!(body["args"]["command"], json!(["node", "s.js"]));
        let ttl =
            request_body(REGISTER, json!({"slug":"preview","idle_ttl_minutes":480})).unwrap();
        assert_eq!(ttl["args"]["idleTtlMinutes"], 480);
        let update = request_body(UPDATE, json!({"slug":"web","path":null})).unwrap();
        assert_eq!(update["args"]["path"], Value::Null);
        assert!(request_body(REGISTER, json!({"slug":"web","path":null})).is_err());
        let tiled = request_body(
            REGISTER,
            json!({"slug":"web","tile":{"title":"Build"},"tile_path":"/tile.json"}),
        )
        .unwrap();
        assert_eq!(tiled["args"]["tile"], json!({"title":"Build"}));
        assert_eq!(tiled["args"]["tilePath"], "/tile.json");
        let cleared = request_body(UPDATE, json!({"slug":"web","tile":null,"tile_path":null})).unwrap();
        assert_eq!(cleared["args"]["tile"], Value::Null);
        assert_eq!(cleared["args"]["tilePath"], Value::Null);
        assert!(request_body(REGISTER, json!({"slug":"web","tile":null})).is_err());
        assert!(request_body(UNREGISTER, json!({"slug":"web","tile":{}})).is_err());
        assert!(request_body(LIST, json!({"tile_path":"/x"})).is_err());
        assert!(request_body(REGISTER, json!({"display_name":"x"})).is_err());
        assert!(
            request_body(REGISTER, json!({"slug":"web","owner_cutex_session_id":"x"})).is_err()
        );
        assert!(request_body(UNREGISTER, json!({"slug":"web","port":3})).is_err());
        assert_eq!(
            request_body(UNREGISTER, json!({"slug":"web"})).unwrap()["op"],
            "unregister"
        );
        assert_eq!(
            request_body(RESTART, json!({"slug":"web"})).unwrap(),
            json!({"op":"restart","args":{"slug":"web"}})
        );
        assert!(request_body(RESTART, json!({"slug":"web","command":"x"})).is_err());
        assert!(request_body(RESTART, json!({})).is_err());
        let scoped = request_body(REGISTER, json!({"slug":"board","scope":"project"})).unwrap();
        assert_eq!(scoped["args"]["ownerScope"], "project");
        assert!(scoped["args"].get("scope").is_none());
        assert_eq!(
            request_body(UPDATE, json!({"slug":"board","scope":"project"})).unwrap()["args"]["ownerScope"],
            "project"
        );
        assert!(request_body(REGISTER, json!({"slug":"board","scope":"team"})).is_err());
        assert!(request_body(UNREGISTER, json!({"slug":"board","scope":"project"})).is_err());
        assert!(request_body(LIST, json!({"scope":"project"})).is_err());
        assert_eq!(
            request_body(LIST, json!({})).unwrap(),
            json!({"op":"list","args":{}})
        );
        assert_eq!(request_body(LIST, Value::Null).unwrap()["op"], "list");
        assert!(request_body(LIST, json!({"slug":"x"})).is_err());
        assert_eq!(
            request_body(LIST, json!({"mine":true})).unwrap()["args"]["mine"],
            true
        );
    }
}
