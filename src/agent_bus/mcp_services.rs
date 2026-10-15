//! `service_*` MCP tools: agents register the web services they start.
//! Caller identity comes from the authenticated runtime (Agent Bus), never
//! from arguments; the Bus serves `POST /api/services/v1`.

use serde::Deserialize;
use serde_json::json;
use serde_json::Map;
use serde_json::Value;

pub(super) const REGISTER: &str = "service_register";
pub(super) const UPDATE: &str = "service_update";
pub(super) const UNREGISTER: &str = "service_unregister";
pub(super) const LIST: &str = "service_list";
pub(super) const PATH: &str = "/api/services/v1";

pub(super) fn is_tool(name: &str) -> bool {
    matches!(name, REGISTER | UPDATE | UNREGISTER | LIST)
}

fn command_schema() -> Value {
    json!({"description":"For runtime managed: a shell command line, or an argv array. `{port}` is replaced by the service port and PORT is set; the server must listen on that port.","anyOf":[{"type":"string"},{"type":"array","items":{"type":"string"}}]})
}

pub(super) fn tools() -> Vec<Value> {
    vec![
        json!({"name":REGISTER,"description":"Register a web server or static HTML report (dev server, preview, API, docs) so the owner can find, open and monitor it. Do this instead of picking a random port: omit `port` and cutex allocates a free one and returns it, then start your server on that port, bound to 127.0.0.1 (managed commands get PORT and HOST=127.0.0.1). With runtime `external` (default) you run the process and cutex only health-checks it; with `managed` cutex runs `command`, restarts it on crashes, and starts it again when the owner opens it after a stop. To show the owner a single page, chart or picture, publish it with artifact_publish show: true instead (it opens beside your conversation); services are for real servers and multi-file sites. A static service serves an absolute root (directory or .html file, no port/command/env). Vite commands must use --port {port} --strictPort. New services default to ephemeral and are reclaimed after idle/TTL or owner archive; persistent may need owner approval (owner setting; same approval_required flow as below). Link the returned cutex:service/ link. reveal requests an owner notification. Re-registering your own slug updates it. Unregister it when the service is gone for good. A `managed` command runs outside your sandbox, so unless you already run with full access and approval `never` it needs the owner's approval: the call answers `approval_required` with an approval_id; retry the same call after the owner approves (end your turn or set an alarm instead of polling).","inputSchema":{"type":"object","properties":{
            "slug":{"type":"string","description":"Stable id, unique on this host: a-z, 0-9 and '-', e.g. `myapp-web`."},
            "display_name":{"type":"string","description":"Human name; defaults to the slug."},
            "reveal":{"type":"boolean","description":"Request that the owner see this page (rate limited)."},
            "port":{"type":"integer","minimum":1,"maximum":65535,"description":"Omit to get a free port allocated."},
            "path":{"type":"string","description":"URL path to open and health-check, e.g. `/` or `/health`."},
            "runtime":{"type":"string","enum":["external","managed","static"],"description":"Default external."},
            "root":{"type":"string","description":"Absolute static directory or .html file in your cwd or project workspace."},
            "lifetime":{"type":"string","enum":["ephemeral","persistent"]},
            "command":command_schema(),
            "cwd":{"type":"string","description":"Absolute working directory for managed."},
            "env":{"type":"object","additionalProperties":{"type":"string"},"description":"Extra environment for managed (values are never shown back)."}
        },"required":["slug"],"additionalProperties":false}}),
        json!({"name":UPDATE,"description":"Change a service you registered (port, path, name, command, cwd, env, lifetime). Upgrading lifetime to persistent may require owner approval (owner setting); downgrade only your own service. Only your own services. `path`/`cwd`/`command` null clears; env entries merge, null removes a key. Changing what a managed service runs (runtime, command, cwd, env) needs the owner's approval like registering it.","inputSchema":{"type":"object","properties":{
            "slug":{"type":"string"},
            "display_name":{"type":"string"},
            "port":{"type":"integer","minimum":1,"maximum":65535},
            "path":{"type":["string","null"]},
            "runtime":{"type":"string","enum":["external","managed","static"]},
            "root":{"type":"string","description":"Absolute static directory or .html file in your cwd or project workspace."},
            "lifetime":{"type":"string","enum":["ephemeral","persistent"]},
            "command":command_schema(),
            "cwd":{"type":["string","null"]},
            "env":{"type":"object","additionalProperties":{"type":["string","null"]}}
        },"required":["slug"],"additionalProperties":false}}),
        json!({"name":UNREGISTER,"description":"Remove a service you registered (managed ones are stopped). Only your own services.","inputSchema":{"type":"object","properties":{"slug":{"type":"string"}},"required":["slug"],"additionalProperties":false}}),
        json!({"name":LIST,"description":"List web services you may use, with lifetime, cutex:service/ link, port, URL and health (reachable/unreachable/starting/stopped): your own, your project's (outside a project: those of the cutex agents you created and the ones they created; native subagents are not cutex agents) and the owner's shared ones. Command, cwd and env keys are shown only for your own services. Check it before starting a server to reuse an existing one.","inputSchema":{"type":"object","properties":{"mine":{"type":"boolean","description":"Only services you registered."}},"required":[],"additionalProperties":false}}),
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
    reveal: Option<bool>,
    #[serde(default, deserialize_with = "present")]
    command: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    cwd: Option<Value>,
    #[serde(default)]
    env: Option<Value>,
    #[serde(default)]
    mine: Option<bool>,
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
                && parsed.reveal.is_none(),
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
    if op == "unregister" {
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
                && parsed.reveal.is_none(),
            "unregister takes only `slug`"
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
    if let Some(value) = parsed.reveal {
        anyhow::ensure!(op == "register", "reveal is only for register");
        out.insert("reveal".into(), json!(value));
    }
    if let Some(value) = parsed.runtime {
        out.insert("runtime".into(), json!(value));
    }
    for (key, value) in [
        ("path", parsed.path),
        ("command", parsed.command),
        ("cwd", parsed.cwd),
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
        let update = request_body(UPDATE, json!({"slug":"web","path":null})).unwrap();
        assert_eq!(update["args"]["path"], Value::Null);
        assert!(request_body(REGISTER, json!({"slug":"web","path":null})).is_err());
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
