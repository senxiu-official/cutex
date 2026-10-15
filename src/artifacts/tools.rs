//! Agent tools `artifact_publish` / `artifact_list` / `artifact_read`
//! (contract "MCP tools"). Served on the Agent Bus delegation route
//! (`/api/asks/v1`) next to the asks tools: the MCP fence gives the caller,
//! arguments never carry identity, and provenance (turn, delegation,
//! runtime generation, profile / model) comes from the server side.
//!
//! Read rule: an agent reads its own project's scope and its own session
//! scope; anything else looks like an unknown artifact.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use super::host::ArtifactHost;
use super::publish::{PublishError, PublishRequest};
use super::store::{Archived, ListFilter};
use super::{ArtifactSummary, Producer, Scope};
use crate::asks::tools::ToolRuntime;

pub const ARTIFACT_PUBLISH: &str = "artifact_publish";
pub const ARTIFACT_LIST: &str = "artifact_list";
pub const ARTIFACT_READ: &str = "artifact_read";
pub const TOOLS: &[&str] = &[ARTIFACT_PUBLISH, ARTIFACT_LIST, ARTIFACT_READ];
/// Page size of `artifact_read` (as `read_overlength_content`).
pub const PAGE_BYTES: usize = crate::documents::PAGE_BYTES;
const LIST_DEFAULT: usize = 20;
const LIST_MAX: usize = 50;

pub fn is_tool(name: &str) -> bool {
    TOOLS.contains(&name)
}

const ACTION_ID: &str = "Idempotency key. Optional: a default is derived from this call and the file's current state. Reuse the echoed action_id when retrying after response_uncertain.";

/// Tool definitions worded for `runtime`.
pub fn definitions(runtime: ToolRuntime) -> Vec<Value> {
    let (reply, files) = match runtime {
        ToolRuntime::Codex => ("your final message", "your shell or file tools"),
        ToolRuntime::Claude => ("your reply", "your Read tool"),
    };
    vec![
        json!({"name": ARTIFACT_PUBLISH,
            "description": format!("Publish a file as an artifact: a durable, versioned result the owner and other agents can open (report, figure, table, data, plan, handoff). Publish results meant for the owner or other agents with this tool instead of pasting them, and cite the returned markdown link in {reply}. After you change the file, publish it again under the same name: that adds a new version (unchanged content adds nothing). The file must be inside your working directory, your project's workspace or a writable root. In a project the artifact belongs to the project; otherwise to you. To show the owner something now (a page, chart, image, drawing), publish one self-contained file (HTML with inline CSS/JS, SVG, PNG, markdown) with show: true: it opens beside your conversation in the owner's desktop app; publishing it again updates the open view. Real servers (dev servers, multi-file sites, pages that need a backend) are services (service_register), not artifacts."),
            "inputSchema": {"type": "object", "properties": {
                "path": {"type": "string", "minLength": 1, "maxLength": 4096, "description": "The file: absolute, or relative to your working directory."},
                "name": {"type": "string", "maxLength": super::MAX_NAME_CHARS, "description": "Artifact name (default: the path relative to the project workspace, else your working directory). The same name publishes a new version of the same artifact."},
                "kind": {"type": "string", "enum": super::KINDS, "description": "Default: from the file type."},
                "note": {"type": "string", "maxLength": super::MAX_NOTE_CHARS, "description": "What this version is or what changed."},
                "show": {"type": "boolean", "description": "Open it beside your conversation in the owner's desktop app now (default false). Rate limited: shown is false with reason rate_limited when you show too often; the publish still succeeds."},
                "action_id": {"type": "string", "maxLength": 128, "description": ACTION_ID}
            }, "required": ["path"], "additionalProperties": false}}),
        json!({"name": ARTIFACT_LIST,
            "description": "List artifacts you can read, newest first: your project's (scope project, the default in a project) or your own (scope mine: your own scope and the project artifacts you produced). Each item has a ref (af-…@<version>) for artifact_read.",
            "inputSchema": {"type": "object", "properties": {
                "scope": {"type": "string", "enum": ["project", "mine"]},
                "q": {"type": "string", "maxLength": 200, "description": "Text in the name or the latest note."},
                "kind": {"type": "string", "enum": super::KINDS},
                "limit": {"type": "integer", "minimum": 1, "maximum": LIST_MAX, "description": "Default 20."}
            }, "additionalProperties": false}}),
        json!({"name": ARTIFACT_READ,
            "description": format!("Read a text artifact one page (up to 8 KB) at a time; binary artifacts (images, PDFs) return their metadata only. ref: af-…@<version> or af-…@latest, or a cutex:artifact/… link. This reads artifacts, not files: to read files, use {files}."),
            "inputSchema": {"type": "object", "properties": {
                "ref": {"type": "string", "minLength": 1, "maxLength": 200, "description": "af-<32 hex>@<version>, af-…@latest or cutex:artifact/<ref>."},
                "cursor": {"type": "integer", "minimum": 0, "description": "Byte offset. Omit it for the first page; to continue, pass next_cursor from the previous result (null means you have reached the end)."}
            }, "required": ["ref"], "additionalProperties": false}}),
    ]
}

/// Who is calling, from the server side.
#[derive(Clone, Debug, Default)]
pub struct Caller {
    pub session: String,
    pub name: Option<String>,
    pub profile: Option<String>,
    pub model: Option<String>,
    pub runtime_generation: Option<u64>,
    pub project: Option<String>,
    pub turn_id: Option<String>,
    pub ask_id: Option<String>,
}

impl Caller {
    fn scope(&self) -> Scope {
        Scope::of_session(&self.session, self.project.as_deref())
    }

    /// Scopes this agent may read.
    fn readable(&self) -> Vec<Scope> {
        let mut scopes = Vec::new();
        if let Some(project) = &self.project {
            scopes.push(Scope::project(project));
        }
        scopes.push(Scope::session(&self.session));
        scopes
    }

    fn may_read(&self, scope: &Scope) -> bool {
        self.readable().contains(scope)
    }

    fn producer(&self) -> Producer {
        Producer {
            kind: "agent".into(),
            session_id: Some(self.session.clone()),
            display_name: self.name.clone(),
            runtime_generation: self.runtime_generation,
            turn_id: self.turn_id.clone(),
            ask_id: self.ask_id.clone(),
            profile: self.profile.clone(),
            model: self.model.clone(),
            attribution: None,
            candidates: Vec::new(),
        }
    }
}

fn refusal(code: &str, detail: impl Into<String>) -> Value {
    json!({"ok": false, "status": "no_write", "code": code, "detail": detail.into()})
}

fn failure(code: &str, detail: impl Into<String>) -> Value {
    json!({"ok": false, "code": code, "detail": detail.into()})
}

fn arguments<T: for<'de> Deserialize<'de>>(args: Value) -> Result<T, String> {
    serde_json::from_value(if args.is_null() { json!({}) } else { args }).map_err(|error| crate::asks::truncate_chars(&error.to_string(), 300))
}

/// Handle one call (pure apart from the host).
pub fn handle(host: &dyn ArtifactHost, caller: &Caller, tool: &str, args: Value, action_id: &str, now: DateTime<Utc>) -> Value {
    match tool {
        ARTIFACT_PUBLISH => publish(host, caller, args, action_id, now),
        ARTIFACT_LIST => list(host, caller, args),
        ARTIFACT_READ => read(host, caller, args),
        other => failure("unknown_tool", format!("{other} is not an artifact tool")),
    }
}

/// The Agent Bus delegation route's entry point.
pub fn handle_from_asks(
    ctx: &crate::asks::tools::ToolContext<'_>,
    tool: &str,
    args: Value,
    action_id: &str,
    turn_id: Option<String>,
) -> Value {
    let info = ctx.world.directory().ok().and_then(|directory| directory.get(ctx.caller).cloned());
    let state = ctx.store.peek().ok().flatten();
    let lease = state.as_ref().and_then(|state| state.leases.get(ctx.caller));
    let ask_id = lease.and_then(|lease| match &lease.holder {
        crate::asks::model::LeaseHolder::Ask { ask_id, .. } => Some(ask_id.clone()),
        _ => None,
    });
    // The call's own turn (Codex `_meta`), else the running turn cutex saw.
    let turn_id = turn_id
        .filter(|turn| !turn.trim().is_empty())
        .or_else(|| state.as_ref().and_then(|state| state.presence.get(ctx.caller).and_then(|presence| presence.turn_id.clone())))
        .or_else(|| lease.and_then(|lease| lease.turn_id.clone()));
    let caller = Caller {
        session: ctx.caller.to_string(),
        name: info.as_ref().and_then(|info| info.name.clone()),
        profile: info.as_ref().and_then(|info| info.profile.clone()),
        model: info.as_ref().and_then(|info| info.model.clone()),
        runtime_generation: info.as_ref().map(|info| info.runtime_generation).filter(|generation| *generation > 0),
        project: info.as_ref().and_then(|info| info.project.clone()),
        turn_id,
        ask_id,
    };
    match ctx.world.artifact_host() {
        Some(host) => handle(host, &caller, tool, args, action_id, ctx.now),
        None => handle(&super::host::BusHost { asks: ctx.store }, &caller, tool, args, action_id, ctx.now),
    }
}

fn publish(host: &dyn ArtifactHost, caller: &Caller, args: Value, action_id: &str, now: DateTime<Utc>) -> Value {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Args {
        path: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        note: Option<String>,
        #[serde(default)]
        show: bool,
    }
    let mut action_id = action_id.to_string();
    let result = (|| -> Result<Value, Value> {
        let args: Args = arguments(args).map_err(|error| refusal("invalid_arguments", error))?;
        let paths = host.session_paths(&caller.session);
        let project_root = caller.project.as_deref().and_then(|project| host.project_root(project));
        let mut roots: Vec<std::path::PathBuf> = Vec::new();
        roots.extend(paths.cwd.clone());
        roots.extend(project_root.clone());
        roots.extend(paths.writable_roots.iter().cloned());
        let mut name_bases = Vec::new();
        name_bases.extend(project_root.clone());
        name_bases.extend(paths.cwd.clone());
        let request = PublishRequest {
            scope: caller.scope(),
            path: args.path.clone(),
            base: paths.cwd.clone(),
            roots: roots.clone(),
            name_bases,
            name: args.name.clone(),
            kind: args.kind.clone(),
            note: args.note.clone(),
            producer: caller.producer(),
        };
        let resolved = super::paths::resolve(&args.path, paths.cwd.as_deref(), &roots).map_err(|error| match error {
            super::paths::PathError::Invalid(detail) => refusal("invalid_arguments", detail),
            super::paths::PathError::Forbidden(detail) => refusal("artifact_path_forbidden", detail),
        })?;
        // The derived default follows the file: the same call after the
        // file changed in the same turn is a new action.
        let fingerprint = format!("{}|{}|{}", resolved.display(), resolved.bytes, resolved.mtime_ms.unwrap_or_default());
        if action_id.starts_with("auto-") {
            let digest = crate::asks::sha256_hex(format!("{action_id}\u{1f}{fingerprint}").as_bytes());
            action_id = format!("auto-{}", &digest[..32]);
        }
        let digest = crate::approvals::request_digest(&json!({
            "tool": ARTIFACT_PUBLISH, "path": args.path, "name": args.name, "kind": args.kind, "note": args.note, "file": fingerprint,
            "show": args.show,
        }));
        let store = host.store().map_err(|error| refusal("persistence_unavailable", crate::asks::truncate_chars(&format!("{error:#}"), 300)))?;
        match store.replay(&caller.session, &action_id, &digest) {
            Ok(Ok(Some(result))) => return Ok(result),
            Ok(Err(())) => return Err(refusal("idempotency_conflict", "this action_id was used for a different request; use a new one")),
            Ok(Ok(None)) => {}
            Err(error) => return Err(refusal("persistence_unavailable", crate::asks::truncate_chars(&format!("{error:#}"), 300))),
        }
        let outcome = super::publish::publish(host, &request, now).map_err(|error| match error {
            PublishError::Invalid(detail) => refusal("invalid_arguments", detail),
            PublishError::Forbidden(detail) => refusal("artifact_path_forbidden", detail),
            PublishError::QuotaFull { .. } => refusal(
                "artifact_quota_exceeded",
                "The artifact store is full and the owner was told. Nothing was published: name the file path in your reply instead.",
            ),
            PublishError::Store(error) => refusal("persistence_unavailable", crate::asks::truncate_chars(&format!("{error:#}"), 300)),
        })?;
        let markdown = super::markdown(&outcome.artifact.name, outcome.version.version, &outcome.version.reference);
        let mut detail = if outcome.new_version {
            format!("Published {} version {}. Cite it with the markdown link.", outcome.artifact.name, outcome.version.version)
        } else {
            format!("Unchanged since version {}: nothing new was published. Cite it with the markdown link.", outcome.version.version)
        };
        // After the publish committed: a limited show never fails the publish.
        let shown = args.show && super::show::allow(&caller.session);
        if shown {
            host.emit(super::show::EVENT_SHOW_REQUESTED, super::show::show_event(&outcome, &caller.session, caller.name.as_deref(), now));
            detail.push_str(" The owner's desktop app was asked to show it beside your conversation.");
        } else if args.show {
            detail.push_str(" Not shown: too many show requests in a short time (at most 3 at once, then 1 per 5 s). The owner can still open the link.");
        }
        let mut result = json!({
            "ok": true,
            "artifact_id": outcome.artifact.artifact_id,
            "name": outcome.artifact.name,
            "version": outcome.version.version,
            "ref": outcome.version.reference,
            "link": outcome.version.link,
            "markdown": markdown,
            "new_version": outcome.new_version,
            "scope": outcome.artifact.scope,
            "shown": shown,
            "detail": detail,
        });
        if args.show && !shown {
            result["reason"] = json!("rate_limited");
        }
        if let Err(error) = store.remember(&caller.session, &action_id, &digest, &result, now) {
            eprintln!("artifacts: idempotency record not written: {error:#}");
        }
        Ok(result)
    })();
    let mut value = result.unwrap_or_else(|error| error);
    value["action_id"] = json!(action_id);
    value
}

fn producer_label(producer: &Producer) -> String {
    producer
        .display_name
        .clone()
        .or_else(|| producer.session_id.clone())
        .unwrap_or_else(|| if producer.kind == "auto" { "auto capture".into() } else { producer.kind.clone() })
}

fn list_item(artifact: &ArtifactSummary) -> Value {
    json!({
        "artifact_id": artifact.artifact_id,
        "name": artifact.name,
        "kind": artifact.kind,
        "latest_version": artifact.latest_version,
        "ref": artifact.latest.reference,
        "updated_at": artifact.updated_at,
        "producer": producer_label(&artifact.latest.producer),
    })
}

fn list(host: &dyn ArtifactHost, caller: &Caller, args: Value) -> Value {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Args {
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        q: Option<String>,
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        limit: Option<u64>,
    }
    let args: Args = match arguments(args) {
        Ok(args) => args,
        Err(error) => return failure("invalid_arguments", error),
    };
    if args.kind.as_deref().is_some_and(|kind| !super::is_kind(kind)) {
        return failure("invalid_arguments", format!("kind: one of {}", super::KINDS.join(", ")));
    }
    let limit = args.limit.map(|limit| limit.clamp(1, LIST_MAX as u64) as usize).unwrap_or(LIST_DEFAULT);
    let store = match host.store() {
        Ok(store) => store,
        Err(error) => return failure("persistence_unavailable", crate::asks::truncate_chars(&format!("{error:#}"), 300)),
    };
    let base = ListFilter { q: args.q.clone(), kind: args.kind.clone(), archived: Archived::No, limit, ..ListFilter::default() };
    let scope = args.scope.as_deref().unwrap_or(if caller.project.is_some() { "project" } else { "mine" });
    let items = match scope {
        "project" => match &caller.project {
            Some(project) => store.list(&ListFilter { scopes: Some(vec![Scope::project(project)]), ..base }).map(|(items, _)| items),
            None => {
                return json!({"ok": true, "items": [], "detail": "You are not in a project; scope mine lists your own artifacts."})
            }
        },
        "mine" => {
            let own = store.list(&ListFilter { scopes: Some(vec![Scope::session(&caller.session)]), ..base.clone() });
            let produced = match &caller.project {
                Some(project) => store
                    .list(&ListFilter { scopes: Some(vec![Scope::project(project)]), producer: Some(caller.session.clone()), ..base })
                    .map(|(items, _)| items),
                None => Ok(Vec::new()),
            };
            own.and_then(|(mut own, _)| {
                own.extend(produced?);
                own.sort_by(|left, right| right.updated_at.cmp(&left.updated_at).then(right.artifact_id.cmp(&left.artifact_id)));
                own.truncate(limit);
                Ok(own)
            })
        }
        _ => return failure("invalid_arguments", "scope: project or mine"),
    };
    match items {
        Ok(items) => json!({"ok": true, "items": items.iter().map(list_item).collect::<Vec<_>>()}),
        Err(error) => failure("persistence_unavailable", crate::asks::truncate_chars(&error.to_string(), 300)),
    }
}

fn read(host: &dyn ArtifactHost, caller: &Caller, args: Value) -> Value {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Args {
        #[serde(rename = "ref")]
        reference: String,
        #[serde(default)]
        cursor: Option<u64>,
    }
    let args: Args = match arguments(args) {
        Ok(args) => args,
        Err(error) => return failure("invalid_arguments", error),
    };
    let Some((artifact_id, which)) = super::parse_ref(&args.reference) else {
        return failure("invalid_arguments", "ref: af-<32 hex>@<version> or af-…@latest");
    };
    let store = match host.store() {
        Ok(store) => store,
        Err(error) => return failure("persistence_unavailable", crate::asks::truncate_chars(&format!("{error:#}"), 300)),
    };
    let found = match store.version(&artifact_id, which) {
        Ok(found) => found.filter(|(artifact, _)| caller.may_read(&artifact.scope)),
        Err(error) => return failure("persistence_unavailable", crate::asks::truncate_chars(&format!("{error:#}"), 300)),
    };
    let Some((artifact, version)) = found else {
        return failure("artifact_not_found", "no such artifact you may read (your project's and your own artifacts only)");
    };
    let mut value = json!({
        "ok": true,
        "artifact_id": artifact.artifact_id,
        "name": artifact.name,
        "kind": artifact.kind,
        "version": version.version,
        "latest_version": artifact.latest_version,
        "ref": version.reference,
        "markdown": super::markdown(&artifact.name, version.version, &version.reference),
        "media_type": version.media_type,
        "bytes": version.bytes,
        "source_path": version.source_path,
        "note": version.note,
        "producer": producer_label(&version.producer),
        "created_at": version.created_at,
        "cursor": null,
        "next_cursor": null,
        "text": null,
    });
    let located = super::read::locate(&store, &version);
    let Some(path) = located.path else {
        value["detail"] = json!(if located.external_changed == Some(true) {
            "The content is not stored in cutex (a large file registered by path) and the file changed or is gone since this version."
        } else {
            "The content of this version is unavailable."
        });
        return value;
    };
    if !super::paths::is_text_media(&version.media_type) {
        value["binary"] = json!(true);
        value["detail"] = json!("Binary artifact: metadata only. The owner can open it in the desktop.");
        return value;
    }
    let cursor = args.cursor.unwrap_or(0);
    let (bytes, total) = match super::read::window(&path, cursor, PAGE_BYTES) {
        Ok(window) => window,
        Err(error) => return failure("persistence_unavailable", crate::asks::truncate_chars(&error.to_string(), 300)),
    };
    let more = cursor + (bytes.len() as u64) < total;
    let Some(text) = super::read::text_of(&bytes, more) else {
        value["binary"] = json!(true);
        value["detail"] = json!("Not UTF-8 text: metadata only.");
        return value;
    };
    let end = cursor + text.len() as u64;
    value["cursor"] = json!(cursor.min(total));
    value["next_cursor"] = if end < total { json!(end) } else { Value::Null };
    value["text"] = json!(text);
    value
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::host::tests::TestHost;
    use super::*;

    pub fn caller(session: &str, project: Option<&str>) -> Caller {
        Caller {
            session: session.into(),
            name: Some("Eva".into()),
            profile: Some("Zephyr".into()),
            model: Some("glm-5".into()),
            runtime_generation: Some(3),
            project: project.map(str::to_string),
            turn_id: Some("turn-1".into()),
            ask_id: Some("ask-1".into()),
        }
    }

    #[test]
    fn definitions_are_closed_and_runtime_worded() {
        for runtime in [ToolRuntime::Codex, ToolRuntime::Claude] {
            let tools = definitions(runtime);
            assert_eq!(tools.len(), 3);
            for tool in &tools {
                assert_eq!(tool["inputSchema"]["additionalProperties"], false);
                let properties = tool["inputSchema"]["properties"].as_object().unwrap();
                for key in properties.keys() {
                    assert!(!["caller", "token", "threadId", "thread_id", "session"].contains(&key.as_str()));
                }
            }
        }
        assert!(definitions(ToolRuntime::Claude)[2]["description"].as_str().unwrap().contains("Read tool"));
        assert!(definitions(ToolRuntime::Codex)[0]["description"].as_str().unwrap().contains("final message"));
    }

    #[test]
    fn publish_list_read_with_provenance_and_scopes() {
        let host = TestHost::new("tools");
        let workspace = host.workspace("ws");
        let elsewhere = host.workspace("cwd-b");
        std::fs::create_dir_all(workspace.join("reports")).unwrap();
        std::fs::write(workspace.join("reports/r.md"), "line one\nline two\n").unwrap();
        std::fs::write(workspace.join("fig.png"), [0x89, b'P', b'N', b'G', 0]).unwrap();
        host.set_session("cutex.a", &workspace.join("reports"));
        host.set_session("cutex.b", &elsewhere);
        host.set_project_root("proj-a", &workspace);
        let a = caller("cutex.a", Some("proj-a"));
        let now = Utc::now();
        let published = handle(&host, &a, ARTIFACT_PUBLISH, json!({"path": "r.md", "note": "first"}), "auto-1", now);
        assert_eq!(published["ok"], true, "{published}");
        assert_eq!(published["version"], 1);
        assert_eq!(published["new_version"], true);
        assert_eq!(published["scope"], json!({"kind": "project", "id": "proj-a"}));
        let reference = published["ref"].as_str().unwrap().to_string();
        assert_eq!(published["markdown"], format!("[r.md v1](cutex:artifact/{reference})"));
        assert_eq!(published["link"], format!("cutex:artifact/{reference}"));
        assert!(published["action_id"].as_str().unwrap().starts_with("auto-"));
        assert_ne!(published["action_id"], "auto-1", "the derived id follows the file");
        let (event_method, event) = host.events.borrow()[0].clone();
        assert_eq!(event_method, super::super::EVENT_PUBLISHED);
        assert_eq!(event["producer"]["turnId"], "turn-1");
        assert_eq!(event["producer"]["askId"], "ask-1");
        assert_eq!(event["producer"]["runtimeGeneration"], 3);
        assert_eq!(event["producer"]["profile"], "Zephyr");
        assert_eq!(event["name"], "reports/r.md", "named relative to the project workspace");
        // The same call replays; a model action id reused for another request conflicts.
        let replay = handle(&host, &a, ARTIFACT_PUBLISH, json!({"path": "r.md", "note": "first"}), "auto-1", now);
        assert_eq!(replay["artifact_id"], published["artifact_id"]);
        assert_eq!(host.events.borrow().len(), 1, "a replay publishes nothing");
        handle(&host, &a, ARTIFACT_PUBLISH, json!({"path": "r.md"}), "mine-1", now);
        let conflict = handle(&host, &a, ARTIFACT_PUBLISH, json!({"path": "r.md", "note": "x"}), "mine-1", now);
        assert_eq!(conflict["code"], "idempotency_conflict");
        // Editing the file: the same derived call is a new action and version.
        std::fs::write(workspace.join("reports/r.md"), "line one\nline 2\n").unwrap();
        let filetime = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options().write(true).open(workspace.join("reports/r.md")).unwrap().set_modified(filetime).unwrap();
        let second = handle(&host, &a, ARTIFACT_PUBLISH, json!({"path": "r.md", "note": "first"}), "auto-1", now);
        assert_eq!((second["version"].as_u64(), second["new_version"].as_bool()), (Some(2), Some(true)), "{second}");
        // Refusals.
        let outside = handle(&host, &a, ARTIFACT_PUBLISH, json!({"path": elsewhere.join("x.md").display().to_string()}), "auto-2", now);
        assert_eq!(outside["code"], "invalid_arguments", "missing file");
        std::fs::write(elsewhere.join("x.md"), "x").unwrap();
        let outside = handle(&host, &a, ARTIFACT_PUBLISH, json!({"path": elsewhere.join("x.md").display().to_string()}), "auto-3", now);
        assert_eq!(outside["code"], "artifact_path_forbidden");
        assert_eq!(outside["status"], "no_write");
        assert_eq!(handle(&host, &a, ARTIFACT_PUBLISH, json!({"path": "r.md", "caller": "x"}), "auto-4", now)["code"], "invalid_arguments");
        // Binary publish, list and read.
        let figure = handle(&host, &a, ARTIFACT_PUBLISH, json!({"path": "../fig.png"}), "auto-5", now);
        assert_eq!(figure["ok"], true, "{figure}");
        let listed = handle(&host, &a, ARTIFACT_LIST, json!({}), "", now);
        assert_eq!(listed["items"].as_array().unwrap().len(), 2, "{listed}");
        assert_eq!(listed["items"][0]["producer"], "Eva");
        let figures = handle(&host, &a, ARTIFACT_LIST, json!({"kind": "figure"}), "", now);
        assert_eq!(figures["items"].as_array().unwrap().len(), 1);
        let read_v1 = handle(&host, &a, ARTIFACT_READ, json!({"ref": reference}), "", now);
        assert_eq!(read_v1["text"], "line one\nline two\n", "{read_v1}");
        assert_eq!(read_v1["next_cursor"], Value::Null);
        let latest = handle(&host, &a, ARTIFACT_READ, json!({"ref": format!("cutex:artifact/{}@latest", published["artifact_id"].as_str().unwrap())}), "", now);
        assert_eq!(latest["text"], "line one\nline 2\n");
        let paged = handle(&host, &a, ARTIFACT_READ, json!({"ref": reference, "cursor": 9}), "", now);
        assert_eq!(paged["text"], "line two\n");
        let binary = handle(&host, &a, ARTIFACT_READ, json!({"ref": figure["ref"]}), "", now);
        assert_eq!((binary["binary"].as_bool(), binary["text"].is_null()), (Some(true), true));
        // Another project's agent cannot list or read them.
        let b = caller("cutex.b", None);
        assert_eq!(handle(&host, &b, ARTIFACT_READ, json!({"ref": reference}), "", now)["code"], "artifact_not_found");
        assert_eq!(handle(&host, &b, ARTIFACT_LIST, json!({}), "", now)["items"], json!([]));
        assert_eq!(handle(&host, &b, ARTIFACT_LIST, json!({"scope": "project"}), "", now)["items"], json!([]));
        // b publishes into its own session scope; "mine" lists it.
        std::fs::write(elsewhere.join("notes.md"), "n").unwrap();
        let own = handle(&host, &b, ARTIFACT_PUBLISH, json!({"path": "notes.md"}), "auto-6", now);
        assert_eq!(own["scope"], json!({"kind": "session", "id": "cutex.b"}));
        assert_eq!(handle(&host, &b, ARTIFACT_LIST, json!({"scope": "mine"}), "", now)["items"][0]["name"], "notes.md");
        // a's "mine": the project artifacts it produced.
        assert_eq!(handle(&host, &a, ARTIFACT_LIST, json!({"scope": "mine"}), "", now)["items"].as_array().unwrap().len(), 2);
        assert_eq!(handle(&host, &a, ARTIFACT_READ, json!({"ref": "nope"}), "", now)["code"], "invalid_arguments");
    }

    #[test]
    fn show_requests_the_side_view_after_publishing_and_is_rate_limited() {
        let host = TestHost::new("show");
        let workspace = host.workspace("ws");
        std::fs::write(workspace.join("doodle.html"), "<svg></svg>").unwrap();
        // A session id of its own: the limiter is process-wide.
        let session = format!("cutex.show-{}", uuid::Uuid::new_v4().simple());
        host.set_session(&session, &workspace);
        let a = caller(&session, None);
        let now = Utc::now();
        let plain = handle(&host, &a, ARTIFACT_PUBLISH, json!({"path": "doodle.html"}), "plain", now);
        assert_eq!((plain["ok"].as_bool(), plain["shown"].as_bool()), (Some(true), Some(false)), "{plain}");
        assert!(plain.get("reason").is_none());
        assert_eq!(host.methods(), [super::super::EVENT_PUBLISHED]);
        // Unchanged content: no new version, but it is shown again.
        let shown = handle(&host, &a, ARTIFACT_PUBLISH, json!({"path": "doodle.html", "show": true}), "show-1", now);
        assert_eq!((shown["shown"].as_bool(), shown["new_version"].as_bool()), (Some(true), Some(false)), "{shown}");
        let events = host.events.borrow().clone();
        let (method, event) = events.last().unwrap();
        assert_eq!(method, super::super::show::EVENT_SHOW_REQUESTED);
        assert_eq!(event["artifactId"], plain["artifact_id"]);
        assert_eq!(event["ref"], shown["ref"]);
        assert_eq!(event["cutexSessionId"], session.as_str());
        assert_eq!(event["displayName"], "Eva");
        assert_eq!(event["mediaType"], "text/html");
        assert_eq!(event["newVersion"], false);
        // A replay of the same action shows nothing new; the same action id with another show conflicts.
        let count = host.events.borrow().len();
        assert_eq!(handle(&host, &a, ARTIFACT_PUBLISH, json!({"path": "doodle.html", "show": true}), "show-1", now)["shown"], true);
        assert_eq!(host.events.borrow().len(), count);
        assert_eq!(handle(&host, &a, ARTIFACT_PUBLISH, json!({"path": "doodle.html"}), "show-1", now)["code"], "idempotency_conflict");
        // Burst of 3, then the publish still succeeds without a show.
        for n in 2..=3 {
            assert_eq!(handle(&host, &a, ARTIFACT_PUBLISH, json!({"path": "doodle.html", "show": true}), &format!("show-{n}"), now)["shown"], true);
        }
        let limited = handle(&host, &a, ARTIFACT_PUBLISH, json!({"path": "doodle.html", "show": true}), "show-4", now);
        assert_eq!((limited["ok"].as_bool(), limited["shown"].as_bool(), limited["reason"].as_str()), (Some(true), Some(false), Some("rate_limited")), "{limited}");
        let shows = host.methods().iter().filter(|m| m.as_str() == super::super::show::EVENT_SHOW_REQUESTED).count();
        assert_eq!(shows, 3);
        // A failed publish never shows.
        let missing = handle(&host, &caller(&format!("{session}-b"), None), ARTIFACT_PUBLISH, json!({"path": "nope.html", "show": true}), "x", now);
        assert_eq!(missing["ok"], false);
        assert!(missing.get("shown").is_none());
    }

    /// End to end through the Agent Bus delegation route (`/api/asks/v1`
    /// body as the MCP facade sends it): identity from the fence, provenance
    /// from the directory, the admission lease and the call's turn.
    #[test]
    fn the_delegation_route_serves_artifact_tools() {
        use crate::asks::store::tests::{temp_store, REQUESTER};
        let (mut world, world_dir) = crate::asks::scheduler::tests::world("artifacts");
        let host = std::rc::Rc::new(TestHost::new("route"));
        let workspace = host.workspace("alpha");
        std::fs::write(workspace.join("plan.md"), "# Plan\n").unwrap();
        host.set_session(REQUESTER, &workspace);
        host.set_project_root("alpha", &workspace);
        world.artifacts = Some(host.clone());
        let (store, store_dir) = temp_store("artifact-route");
        let now = Utc::now();
        store
            .with_state(|state| {
                state.leases.insert(
                    REQUESTER.into(),
                    crate::asks::model::AdmissionLease {
                        cutex_session_id: REQUESTER.into(),
                        holder: crate::asks::model::LeaseHolder::Ask { ask_id: "ask-77".into(), attempt: 1 },
                        native_message_id: "m-1".into(),
                        runtime_generation: 1,
                        turn_id: Some("lease-turn".into()),
                        since: crate::asks::now_rfc3339(now),
                    },
                );
            })
            .unwrap();
        let call = |tool: &str, args: Value, turn: Option<&str>| {
            let send = |_: &crate::asks::tools::TellSend<'_>| Ok(json!({"ok": true}));
            let wake_chain = |_: &str, _: &str, _: &str| None;
            let clock = Utc::now;
            let sleep = || {};
            let ctx = crate::asks::tools::ToolContext {
                store: &store,
                world: &world,
                caller: REQUESTER,
                now,
                send: &send,
                wake_chain: &wake_chain,
                clock: &clock,
                sleep: &sleep,
                wait: true,
                facade: crate::asks::tools::FACADE_DELEGATIONS,
            };
            let mut body = json!({"tool": tool, "arguments": args, "action_id": "auto-0123456789abcdef0123456789abcdef"});
            if let Some(turn) = turn {
                body["turn_id"] = json!(turn);
            }
            crate::asks::tools::handle(&ctx, body.to_string().as_bytes())
        };
        let published = call(ARTIFACT_PUBLISH, json!({"path": "plan.md", "kind": "plan"}), Some("codex-turn-9"));
        assert_eq!(published["ok"], true, "{published}");
        assert_eq!(published["scope"], json!({"kind": "project", "id": "alpha"}));
        let event = host.events.borrow()[0].1.clone();
        assert_eq!(event["producer"]["sessionId"], REQUESTER);
        assert_eq!(event["producer"]["displayName"], "Eva");
        assert_eq!(event["producer"]["turnId"], "codex-turn-9", "the call's own turn wins");
        assert_eq!(event["producer"]["askId"], "ask-77");
        assert_eq!(event["producer"]["model"], "deepseek-v4");
        assert_eq!(event["kind"], "plan");
        // Without call metadata (Claude): the lease's turn.
        std::fs::write(workspace.join("plan.md"), "# Plan v2\n").unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options().write(true).open(workspace.join("plan.md")).unwrap().set_modified(later).unwrap();
        let second = call(ARTIFACT_PUBLISH, json!({"path": "plan.md", "action_id": "model-key-1"}), None);
        assert_eq!(second["version"], 2, "{second}");
        assert_eq!(second["action_id"], "model-key-1");
        assert_eq!(host.events.borrow()[1].1["producer"]["turnId"], "lease-turn");
        let listed = call(ARTIFACT_LIST, json!({}), None);
        assert_eq!(listed["items"][0]["latest_version"], 2, "{listed}");
        let read = call(ARTIFACT_READ, json!({"ref": second["ref"]}), None);
        assert_eq!(read["text"], "# Plan v2\n");
        // Every defined tool is listed for both runtimes by the facade.
        for runtime in [ToolRuntime::Codex, ToolRuntime::Claude] {
            let listed = crate::agent_bus::mcp::tools_for(runtime, true);
            for tool in TOOLS {
                let count = listed["tools"].as_array().unwrap().iter().filter(|item| item["name"] == *tool).count();
                assert_eq!(count, 1, "{tool}");
            }
        }
        let _ = std::fs::remove_dir_all(world_dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }
}
