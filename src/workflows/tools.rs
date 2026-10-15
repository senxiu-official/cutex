//! Agent tools (design §4.3): `workflow_start`, `workflow_read`,
//! `workflow_list`, `workflow_stop`. Served on the delegation route
//! (`POST /api/asks/v1`, Agent Bus process) for the authenticated caller.
//!
//! The Agent Bus process only writes the workflow store and the approval
//! store; the management service (`cutex management serve`) runs the runs:
//! its tick starts approved runs, applies stop requests and announces events.
//! Step sessions get no workflow tools (one level only).

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Value};

use super::engine::{submit_run, ApiError, StartRequest, WorkflowSettings};
use super::facts::LibraryFacts;
use super::model::*;
use super::store::{RunFilter, WorkflowStore};

pub const WORKFLOW_START: &str = "workflow_start";
pub const WORKFLOW_READ: &str = "workflow_read";
pub const WORKFLOW_LIST: &str = "workflow_list";
pub const WORKFLOW_STOP: &str = "workflow_stop";
pub const TOOLS: &[&str] = &[WORKFLOW_START, WORKFLOW_READ, WORKFLOW_LIST, WORKFLOW_STOP];
/// Drafts live in the author's workspace, git-ignored (§4.1).
pub const DRAFT_DIR: &str = ".cutex/workflow-drafts";

pub fn is_tool(name: &str) -> bool {
    TOOLS.contains(&name)
}

const START_DESCRIPTION: &str = "Start a cutex workflow: a JavaScript orchestration script that fans work out to fresh one-shot agents. \
Use it ONLY when the owner explicitly asked for a workflow (\"use a workflow\", 「用工作流」, /workflow); never because a task looks big. \
The owner must approve the run plan (name, phases, profiles, limits) before it starts; the result says pendingApproval — end your turn then. \
When the run ends you get a [workflow completion] message with the result; do not poll.\n\
Script (plain JS, not TypeScript; Claude Code Workflow dialect): it must begin with `export const meta = {name, description, whenToUse?, phases?: [{title, detail}]}` as a pure literal, then a body using top-level await / return:\n\
- agent(prompt, {label?, phase?, schema?, profile?, model?, effort?}) → Promise: the step's final message (string), or with `schema` (JSON Schema, object root, required ⊆ properties) the validated object; null when the step failed. Each call is a new agent with no context but the prompt (it runs in your cwd with your sandbox). Throws agent_limit / budget_exhausted / profile_not_approved.\n\
- pipeline(items, stage1, stage2, …): no barrier, each stage gets (prev, item, index); a failing item becomes null. Prefer it.\n\
- parallel(thunks): barrier; never rejects, failures become null.\n\
- phase(title), log(text), args (the `args` value verbatim), budget {total, spent(), remaining()} (tokens).\n\
Date.now(), new Date() without arguments and Math.random() throw (they would break resume); there is no filesystem, network or timer API — agents read and run things. \
Keep parallel steps read-only and do workspace writes in one step. Use a literal `profile: \"…\"` for any profile other than the default so it shows on the plan. \
Scale the fleet to what the owner asked for. Limits default to 30 agents, 4 at once, 2,000,000 tokens per run.\n\
An inline script is saved to <cwd>/.cutex/workflow-drafts/ (path in the result); edit that file and pass script_path to iterate. \
To continue a stopped or failed run without repeating finished steps, pass resume_from_run_id (same script and args = every finished step is reused).";

pub fn definitions() -> Vec<Value> {
    let limits = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "max_agents": {"type": "integer", "minimum": 1, "maximum": 1000, "description": "Agents (steps incl. schema repairs) this run may start; default 30."},
            "max_concurrency": {"type": "integer", "minimum": 1, "maximum": 16, "description": "Steps at once; default 4 (the host caps all runs together)."},
            "max_tokens": {"type": "integer", "minimum": 1, "description": "Token budget (input + output of all steps); default 2000000."},
            "hard_stop": {"type": "boolean", "description": "Over budget, also stop running steps (default: they finish)."}
        }
    });
    vec![
        json!({
            "name": WORKFLOW_START,
            "description": START_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "script": {"type": "string", "description": "The whole script (export const meta … body)."},
                    "script_path": {"type": "string", "description": "A draft file under your cwd (from an earlier result) instead of script."},
                    "args": {"description": "Any JSON value, passed to the script as `args` verbatim (send arrays / objects as JSON, not as a string)."},
                    "name": {"type": "string", "description": "Display name (default meta.name)."},
                    "limits": limits,
                    "default_profile": {"type": "string", "description": "Profile for steps without opts.profile (default: yours)."},
                    "resume_from_run_id": {"type": "string", "description": "A finished run of yours: its finished steps are reused (same prompt + options)."},
                    "action_id": {"type": "string"}
                }
            }
        }),
        json!({
            "name": WORKFLOW_READ,
            "description": "Read one of your workflow runs now (never waits): state, phases, steps, log tail; include \"steps\" (full prompts and results), \"log\", \"result\" or \"script\" for more.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["run_id"],
                "properties": {
                    "run_id": {"type": "string"},
                    "include": {"type": "array", "items": {"type": "string", "enum": ["steps", "log", "result", "script"]}},
                    "action_id": {"type": "string"}
                }
            }
        }),
        json!({
            "name": WORKFLOW_LIST,
            "description": "List your workflow runs, newest first.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "state": {"type": "string", "enum": ["open", "closed", "all"]},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 100},
                    "action_id": {"type": "string"}
                }
            }
        }),
        json!({
            "name": WORKFLOW_STOP,
            "description": "Stop one of your workflow runs (pending approval or running). Running steps are stopped; finished results stay readable and can be reused with resume_from_run_id.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["run_id"],
                "properties": {"run_id": {"type": "string"}, "action_id": {"type": "string"}}
            }
        }),
    ]
}

fn refusal(code: &str, detail: impl Into<String>) -> Value {
    json!({"ok": false, "status": "no_write", "code": code, "detail": detail.into()})
}

fn api_refusal(error: ApiError) -> Value {
    let mut value = refusal(&error.code, error.message);
    if let (Some(object), Some(details)) = (value.as_object_mut(), error.details.as_object()) {
        for (key, item) in details {
            object.insert(key.clone(), item.clone());
        }
    }
    value
}

fn settings() -> WorkflowSettings {
    crate::config::store::load_codez_config().workflows
}

fn open_store() -> Result<WorkflowStore, Value> {
    let root = WorkflowStore::default_root().map_err(|e| refusal("workflows_unavailable", format!("{e:#}")))?;
    WorkflowStore::open(root).map_err(|e| refusal("workflows_unavailable", format!("{e:#}")))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartArgs {
    #[serde(default)]
    script: Option<String>,
    #[serde(default)]
    script_path: Option<String>,
    #[serde(default)]
    args: Value,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    limits: Option<LimitsRequest>,
    #[serde(default)]
    default_profile: Option<String>,
    #[serde(default)]
    resume_from_run_id: Option<String>,
}

fn slug(name: &str) -> String {
    let slug: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .chars()
        .take(48)
        .collect();
    if slug.is_empty() {
        "workflow".into()
    } else {
        slug
    }
}

/// Write an inline script as the next free `<slug>-<n>.js` draft. None when
/// the cwd is not writable (the result says so; the agent resends inline).
pub fn write_draft(cwd: &Path, name: &str, script: &str) -> Option<PathBuf> {
    let dir = cwd.join(DRAFT_DIR);
    std::fs::create_dir_all(&dir).ok()?;
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        std::fs::write(&ignore, "*\n").ok()?;
    }
    let slug = slug(name);
    for n in 1..10_000 {
        let path = dir.join(format!("{slug}-{n}.js"));
        if path.exists() {
            if std::fs::read_to_string(&path).ok().as_deref() == Some(script) {
                return Some(path);
            }
            continue;
        }
        std::fs::write(&path, script).ok()?;
        return Some(path);
    }
    None
}

/// A draft path must stay inside the author's cwd or writable roots.
fn read_draft(path: &str, roots: &[PathBuf]) -> Result<(String, PathBuf), Value> {
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(refusal("invalid_arguments", "script_path must be absolute"));
    }
    let canonical = std::fs::canonicalize(&path).map_err(|e| refusal("script_unreadable", format!("{}: {e}", path.display())))?;
    let inside = roots.iter().filter_map(|root| std::fs::canonicalize(root).ok()).any(|root| canonical.starts_with(root));
    if !inside {
        return Err(refusal("script_path_outside_workspace", "script_path must be inside your cwd or writable roots"));
    }
    let metadata = std::fs::metadata(&canonical).map_err(|e| refusal("script_unreadable", e.to_string()))?;
    if !metadata.is_file() || metadata.len() as usize > super::script::MAX_SCRIPT_BYTES {
        return Err(refusal("script_unreadable", "script_path must be a file of at most 256 KiB"));
    }
    let text = std::fs::read_to_string(&canonical).map_err(|e| refusal("script_unreadable", e.to_string()))?;
    Ok((text, canonical))
}

fn run_brief(run: &RunRecord) -> Value {
    json!({
        "run_id": run.run_id, "seq": run.seq, "name": run.name, "state": run.state, "stop_reason": run.stop_reason,
        "phase": run.current_phase, "counts": run.counts, "tokens": run.tokens, "limits": run.limits,
        "approval_id": run.approval_id, "created_at": run.created_at, "ended_at": run.ended_at,
        "script_path": run.draft_path, "error": run.error,
    })
}

fn owned_run(store: &WorkflowStore, caller: &str, run_id: &str) -> Result<RunRecord, Value> {
    let run = store
        .run(run_id)
        .map_err(|e| refusal("workflows_unavailable", format!("{e:#}")))?
        .ok_or_else(|| refusal("run_not_found", format!("no workflow run {run_id}")))?;
    if run.author.session() != Some(caller) {
        return Err(refusal("run_not_found", format!("no workflow run {run_id} of yours")));
    }
    Ok(run)
}

/// Handle one tool call for the authenticated `caller` (durable session id).
pub fn handle(caller: &str, tool: &str, arguments: Value, action_id: &str) -> Value {
    let result = match tool {
        WORKFLOW_START => start(caller, arguments, action_id),
        WORKFLOW_READ => read(caller, arguments),
        WORKFLOW_LIST => list(caller, arguments),
        WORKFLOW_STOP => stop(caller, arguments),
        other => Err(refusal("unknown_tool", format!("unknown workflow tool {other}"))),
    };
    let mut value = result.unwrap_or_else(|refusal| refusal);
    if let Some(object) = value.as_object_mut() {
        object.insert("action_id".into(), json!(action_id));
    }
    value
}

fn parse<T: for<'de> Deserialize<'de>>(arguments: Value) -> Result<T, Value> {
    serde_json::from_value(if arguments.is_null() { json!({}) } else { arguments })
        .map_err(|e| refusal("invalid_arguments", e.to_string()))
}

fn start(caller: &str, arguments: Value, action_id: &str) -> Result<Value, Value> {
    let args: StartArgs = parse(arguments)?;
    let store = open_store()?;
    // A retried call (same action id) returns the run it created.
    let previous = store
        .runs(&RunFilter { author_session: Some(caller.to_string()), limit: 50, ..Default::default() })
        .map_err(|e| refusal("workflows_unavailable", format!("{e:#}")))?;
    if let Some(run) = previous.iter().find(|run| run.start_action_id.as_deref() == Some(action_id)) {
        return Ok(start_result(run, &[]));
    }
    let facts = LibraryFacts::new();
    let author = super::engine::Facts::author_facts(&facts, caller).ok_or_else(|| refusal("author_not_found", "your session is unknown to cutex"))?;
    let paths = crate::artifacts::host::session_paths(caller);
    let mut roots: Vec<PathBuf> = vec![PathBuf::from(&author.cwd)];
    roots.extend(paths.writable_roots.iter().cloned());
    let (script, draft_path, draft_note) = match (args.script, args.script_path) {
        (Some(_), Some(_)) => return Err(refusal("invalid_arguments", "pass script or script_path, not both")),
        (Some(script), None) => {
            let name = super::script::check(&script).map(|c| c.meta.name).unwrap_or_else(|_| "workflow".into());
            match write_draft(Path::new(&author.cwd), &name, &script) {
                Some(path) => (script, Some(path.display().to_string()), None),
                None => (script, None, Some("your cwd is not writable: no draft file was saved; resend the script inline to iterate")),
            }
        }
        (None, Some(path)) => {
            let (text, canonical) = read_draft(&path, &roots)?;
            (text, Some(canonical.display().to_string()), None)
        }
        (None, None) => match &args.resume_from_run_id {
            Some(previous) => {
                let run = owned_run(&store, caller, previous)?;
                let text = store.read_script(&run.script_sha256).map_err(|e| refusal("script_unreadable", format!("{e:#}")))?;
                (text, run.draft_path.clone(), None)
            }
            None => return Err(refusal("invalid_arguments", "pass script or script_path")),
        },
    };
    let approvals = crate::approvals::ApprovalStore::open_default().ok();
    let request = StartRequest {
        author: Some(Author::Agent { cutex_session_id: caller.to_string(), name: None }),
        script,
        draft_path: draft_path.clone(),
        args: args.args,
        name: args.name,
        limits: args.limits.unwrap_or_default(),
        default_profile: args.default_profile,
        resume_from_run_id: args.resume_from_run_id,
        cwd: None,
        project_id: None,
        start_action_id: Some(action_id.to_string()),
    };
    let outcome = submit_run(&store, approvals.as_ref(), &facts, &settings(), request).map_err(api_refusal)?;
    let mut result = start_result(&outcome.run, &outcome.warnings);
    if let Some(note) = draft_note {
        result["draft_note"] = json!(note);
    }
    Ok(result)
}

fn start_result(run: &RunRecord, warnings: &[String]) -> Value {
    let next = match run.state {
        RunState::PendingApproval => "Waiting for the owner to approve the plan. End your turn now; the run starts by itself after approval and you will get a [workflow completion] message. Do not poll.",
        RunState::Running => "Running. End your turn or continue other work; you will get a [workflow completion] message. Do not poll.",
        _ => "The run did not start (see state).",
    };
    json!({
        "ok": true,
        "run_id": run.run_id,
        "seq": run.seq,
        "state": run.state,
        "approval_id": run.approval_id,
        "script_path": run.draft_path,
        "limits": run.limits,
        "step_profiles": run.step_profiles,
        "warnings": warnings,
        "next": next,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    run_id: String,
    #[serde(default)]
    include: Vec<String>,
}

fn read(caller: &str, arguments: Value) -> Result<Value, Value> {
    let args: ReadArgs = parse(arguments)?;
    let store = open_store()?;
    let run = owned_run(&store, caller, &args.run_id)?;
    let steps = store.steps(&run.run_id).map_err(|e| refusal("workflows_unavailable", format!("{e:#}")))?;
    let full = args.include.iter().any(|i| i == "steps");
    let mut out = run_brief(&run);
    out["ok"] = json!(true);
    out["description"] = json!(run.description);
    out["phases"] = json!(run.phases);
    out["steps"] = json!(steps
        .iter()
        .map(|s| {
            let mut v = json!({"step_key": s.step_key, "phase": s.phase, "label": s.label, "state": s.state, "tokens": s.tokens, "error": s.error, "activity": s.activity});
            if full {
                v["prompt"] = json!(s.prompt);
                v["result"] = s.result.clone().unwrap_or(Value::Null);
            } else {
                v["result_preview"] = json!(s.result.as_ref().map(|r| r.to_string().chars().take(200).collect::<String>()));
            }
            v
        })
        .collect::<Vec<_>>());
    let log = if args.include.iter().any(|i| i == "log") { store.log_tail(&run.run_id, 200) } else { store.log_tail(&run.run_id, 10) };
    out["log"] = json!(log.unwrap_or_default().iter().map(|l| format!("{} {}", l.level, l.text)).collect::<Vec<_>>());
    if args.include.iter().any(|i| i == "result") {
        out["result"] = run.result.clone().unwrap_or(Value::Null);
    } else if let Some(result) = &run.result {
        out["result_preview"] = json!(result.to_string().chars().take(500).collect::<String>());
    }
    if args.include.iter().any(|i| i == "script") {
        out["script"] = json!(store.read_script(&run.script_sha256).unwrap_or_default());
    }
    Ok(out)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

fn list(caller: &str, arguments: Value) -> Result<Value, Value> {
    let args: ListArgs = parse(arguments)?;
    let open = match args.state.as_deref() {
        None | Some("all") => None,
        Some("open") => Some(true),
        Some("closed") => Some(false),
        Some(other) => return Err(refusal("invalid_arguments", format!("state: open, closed or all (got {other})"))),
    };
    let store = open_store()?;
    let runs = store
        .runs(&RunFilter { open, author_session: Some(caller.to_string()), limit: args.limit.unwrap_or(20).clamp(1, 100), ..Default::default() })
        .map_err(|e| refusal("workflows_unavailable", format!("{e:#}")))?;
    Ok(json!({"ok": true, "runs": runs.iter().map(run_brief).collect::<Vec<_>>()}))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StopArgs {
    run_id: String,
}

fn stop(caller: &str, arguments: Value) -> Result<Value, Value> {
    let args: StopArgs = parse(arguments)?;
    let store = open_store()?;
    let run = owned_run(&store, caller, &args.run_id)?;
    if run.state.is_terminal() {
        return Ok(json!({"ok": true, "run_id": run.run_id, "state": run.state, "detail": "the run had already ended"}));
    }
    store.request_stop(&run.run_id, StopReason::Model).map_err(|e| refusal("workflows_unavailable", format!("{e:#}")))?;
    Ok(json!({"ok": true, "run_id": run.run_id, "state": "stopping", "detail": "stop requested; running steps are being stopped and you will get a [workflow completion] message"}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drafts_are_numbered_ignored_and_reused_for_identical_text() {
        let dir = std::env::temp_dir().join(format!("cutex-wf-draft-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = write_draft(&dir, "Code Review!", "one").unwrap();
        assert!(a.ends_with(".cutex/workflow-drafts/code-review-1.js"), "{}", a.display());
        assert_eq!(write_draft(&dir, "Code Review!", "one").unwrap(), a);
        let b = write_draft(&dir, "Code Review!", "two").unwrap();
        assert!(b.ends_with("code-review-2.js"));
        assert_eq!(std::fs::read_to_string(dir.join(DRAFT_DIR).join(".gitignore")).unwrap(), "*\n");
        assert!(read_draft(b.to_str().unwrap(), &[dir.clone()]).is_ok());
        let outside = read_draft(b.to_str().unwrap(), &[std::env::temp_dir().join("elsewhere")]).unwrap_err();
        assert_eq!(outside["code"], json!("script_path_outside_workspace"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn every_tool_has_a_closed_schema() {
        let defs = definitions();
        assert_eq!(defs.len(), TOOLS.len());
        for def in defs {
            assert_eq!(def["inputSchema"]["additionalProperties"], json!(false), "{}", def["name"]);
            assert!(is_tool(def["name"].as_str().unwrap()));
        }
    }
}
