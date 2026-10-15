//! Management boundary for file-only undo; history access stays in this module.
use super::{
    history::{self, HistoryCallError},
    repository::EventRepository,
    server::{append_cutex_event, require_session_revision, ValidatedCutexRequest},
    user_input::UserInputExecutionError,
};
use crate::files_revert::{journal, plan};
use crate::management::{
    control_plane::HumanManagementPrincipal, server::ManagementRequestContext,
};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::PathBuf;

pub const METHOD: &str = "cutex/session/files/revert";
pub const EVENT: &str = "cutex/session/filesReverted";

#[derive(Debug)]
struct Params {
    turn: String,
    dry: bool,
    paths: Option<Vec<String>>,
    digest: Option<String>,
}

fn error(code: &str, message: impl ToString, details: Value) -> UserInputExecutionError {
    UserInputExecutionError {
        stage: "filesRevert".into(),
        code: code.into(),
        message: message.to_string(),
        retryable: false,
        details,
        outcome_unknown: code == "revert_partial",
    }
}

fn parse(value: &Value) -> Result<Params, UserInputExecutionError> {
    let object = value
        .as_object()
        .ok_or_else(|| error("invalid_request", "params must be an object", json!({})))?;
    if object.keys().any(|k| {
        !matches!(
            k.as_str(),
            "turnId" | "dryRun" | "paths" | "expectedPlanDigest" | "expectedRevision"
        )
    }) {
        return Err(error("invalid_request", "unknown parameter", json!({})));
    }
    let turn = value["turnId"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| error("invalid_request", "turnId is required", json!({})))?
        .to_owned();
    let dry = value["dryRun"]
        .as_bool()
        .ok_or_else(|| error("invalid_request", "dryRun must be a boolean", json!({})))?;
    let digest = object
        .get("expectedPlanDigest")
        .map(|v| {
            v.as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| {
                    error(
                        "invalid_request",
                        "expectedPlanDigest must be a string",
                        json!({}),
                    )
                })
        })
        .transpose()?;
    if !dry && digest.is_none() {
        return Err(error(
            "invalid_request",
            "execution requires expectedPlanDigest",
            json!({}),
        ));
    }
    let paths = object
        .get("paths")
        .map(|v| -> Result<Vec<String>, UserInputExecutionError> {
            v.as_array()
                .filter(|a| !a.is_empty())
                .ok_or_else(|| {
                    error(
                        "invalid_request",
                        "paths must be a non-empty string array",
                        json!({}),
                    )
                })?
                .iter()
                .map(|p| {
                    p.as_str()
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .ok_or_else(|| {
                            error(
                                "invalid_request",
                                "paths contains a non-string or empty path",
                                json!({}),
                            )
                        })
                })
                .collect()
        })
        .transpose()?;
    Ok(Params {
        turn,
        dry,
        paths,
        digest,
    })
}

/// Explicitly reject preview/truncation markers anywhere in the selected turn.
/// No summary is ever promoted into an executable plan.
fn complete(value: &Value) -> bool {
    match value {
        Value::Object(object) => object.iter().all(|(key, value)| {
            if matches!(
                key.as_str(),
                "truncated" | "itemsTruncated" | "isTruncated" | "hasMoreItems"
            ) && value == &json!(true)
            {
                return false;
            }
            if matches!(key.as_str(), "itemsView" | "view") && value == &json!("summary") {
                return false;
            }
            if matches!(key.as_str(), "itemsBackwardsCursor" | "nextItemsCursor")
                && !value.is_null()
            {
                return false;
            }
            complete(value)
        }),
        Value::Array(array) => array.iter().all(complete),
        _ => true,
    }
}

/// Newer turns scanned for `laterTurnsTouchedFiles`; advisory only.
const LATER_SCAN_LIMIT: usize = 500;

#[derive(Debug)]
struct ReadTurn {
    turn: Value,
    later: Vec<Value>,
    unscanned: usize,
}

fn history_error(failure: HistoryCallError) -> UserInputExecutionError {
    match failure {
        HistoryCallError::Rpc(native)
            if native["code"] == -32001
                || native.pointer("/data/code").and_then(Value::as_str)
                    == Some("history_turn_incomplete") =>
        {
            error(
                "turn_history_too_large",
                "the complete turn is unavailable (size limit or native truncation)",
                json!({ "native": native }),
            )
        }
        HistoryCallError::Rpc(native) => error(
            "native_history_unavailable",
            "the native app-server could not read this turn",
            json!({ "native": native }),
        ),
        HistoryCallError::Unavailable(diagnostic) => {
            let mut e = error(
                "codex_app_server_unavailable",
                "the native history reader could not be started",
                json!({ "diagnostic": diagnostic }),
            );
            e.retryable = true;
            e
        }
    }
}

/// Plans only ever come from `history::read_complete_turn`: an incomplete,
/// over-limit or truncated turn is refused, never replaced by a preview.
fn checked(
    read: Option<(history::CompleteHistoryTurn, history::LaterTurns)>,
    turn_id: &str,
) -> Result<ReadTurn, UserInputExecutionError> {
    let Some((complete_turn, later)) = read else {
        return Err(error(
            "turn_not_found",
            "the turn does not exist in the bound thread",
            json!({ "turnId": turn_id }),
        ));
    };
    let turn = complete_turn.turn().clone();
    if turn["id"].as_str() != Some(turn_id) {
        return Err(error(
            "native_history_unavailable",
            "history returned a different turn",
            json!({}),
        ));
    }
    if turn["status"] == "inProgress" {
        return Err(error(
            "turn_in_progress",
            "target turn is still running",
            json!({}),
        ));
    }
    if !complete(&turn) || !turn["items"].is_array() {
        return Err(error(
            "turn_history_too_large",
            "complete turn items are unavailable",
            json!({}),
        ));
    }
    Ok(ReadTurn {
        turn,
        later: later.turns,
        unscanned: later.unscanned,
    })
}

#[cfg(any(test, debug_assertions))]
fn read_turn_with(
    id: &str,
    session: &Value,
    turn_id: &str,
    live: &mut dyn FnMut(&str, Value) -> Result<Value, HistoryCallError>,
    offline: &mut dyn FnMut(&str, Value) -> Result<Value, HistoryCallError>,
) -> Result<ReadTurn, UserInputExecutionError> {
    let (read, _) = history::read_complete_session_turn_with(
        id,
        session,
        turn_id,
        LATER_SCAN_LIMIT,
        live,
        offline,
    )
    .map_err(history_error)?;
    checked(read, turn_id)
}

/// Debug fixture / test native: serves `turns` (oldest first, full items)
/// through `thread/read`, `thread/turns/list` (desc) and `thread/items/list`.
#[cfg(any(test, debug_assertions))]
fn fixture_native<'a>(
    thread: &'a str,
    turns: &'a [Value],
) -> impl FnMut(&str, Value) -> Result<Value, HistoryCallError> + 'a {
    move |method, params| {
        let bad = |message: &str| HistoryCallError::Rpc(json!({"code": -32602, "message": message}));
        if params["threadId"].as_str() != Some(thread) {
            return Err(bad("unknown thread"));
        }
        let offset = match params["cursor"].as_str() {
            Some(cursor) => cursor.parse::<usize>().map_err(|_| bad("bad cursor"))?,
            None => 0,
        };
        let limit = params["limit"].as_u64().unwrap_or(100).clamp(1, 100) as usize;
        match method {
            "thread/read" => {
                let mut thread = json!({ "id": thread });
                if params["includeTurns"] == true {
                    thread["turns"] = json!(turns);
                }
                Ok(json!({ "thread": thread }))
            }
            "thread/turns/list" => {
                let data: Vec<Value> = turns
                    .iter()
                    .rev()
                    .skip(offset)
                    .take(limit)
                    .map(|turn| {
                        let mut turn = turn.clone();
                        if params["itemsView"] == "notLoaded" {
                            turn["items"] = json!([]);
                        }
                        turn
                    })
                    .collect();
                let next = (offset + data.len() < turns.len())
                    .then(|| (offset + data.len()).to_string());
                Ok(json!({ "data": data, "nextCursor": next }))
            }
            "thread/items/list" => {
                let turn_id = params["turnId"].as_str().unwrap_or_default();
                let turn = turns
                    .iter()
                    .find(|turn| turn["id"] == turn_id)
                    .ok_or_else(|| bad("unknown turn"))?;
                let items = turn["items"].as_array().cloned().unwrap_or_default();
                let data: Vec<Value> = items
                    .iter()
                    .skip(offset)
                    .take(limit)
                    .map(|item| json!({ "turnId": turn_id, "item": item }))
                    .collect();
                let next = (offset + data.len() < items.len())
                    .then(|| (offset + data.len()).to_string());
                Ok(json!({ "data": data, "nextCursor": next }))
            }
            _ => Err(HistoryCallError::Rpc(
                json!({"code": -32601, "message": "unknown method"}),
            )),
        }
    }
}

fn read_turn(
    context: ManagementRequestContext,
    id: &str,
    session: &Value,
    turn: &str,
) -> Result<ReadTurn, UserInputExecutionError> {
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os("CUTEX_TEST_FILES_REVERT_HISTORY") {
        let private = std::env::var_os("CUTEX_TEST_PRIVATE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.join(crate::config::paths::PRIVATE_HOME_MARKER).is_file())
            .ok_or_else(|| {
                error(
                    "invalid_request",
                    "history fixture requires a marked private home",
                    json!({}),
                )
            })?;
        let source =
            std::fs::canonicalize(path).map_err(|e| error("invalid_request", e, json!({})))?;
        let boundary =
            std::fs::canonicalize(private).map_err(|e| error("invalid_request", e, json!({})))?;
        let workspace = session["cwd"]
            .as_str()
            .and_then(|p| std::fs::canonicalize(p).ok());
        if !source.starts_with(&boundary) || !workspace.is_some_and(|p| p.starts_with(&boundary)) {
            return Err(error(
                "invalid_request",
                "fixture and workspace must be inside the private home",
                json!({}),
            ));
        }
        let value: Value = serde_json::from_slice(
            &std::fs::read(source).map_err(|e| error("invalid_request", e, json!({})))?,
        )
        .map_err(|e| error("invalid_request", e, json!({})))?;
        let turns = value["turns"].as_array().cloned().ok_or_else(|| {
            error(
                "invalid_request",
                "fixture requires turns in oldest-first order",
                json!({}),
            )
        })?;
        let thread = session
            .pointer("/native/threadId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        return read_turn_with(
            id,
            session,
            turn,
            &mut fixture_native(thread, &turns),
            &mut fixture_native(thread, &turns),
        );
    }
    let read = history::read_complete_session_turn(
        id,
        session,
        turn,
        LATER_SCAN_LIMIT,
        context.forward_native_request,
        context.open_offline_history,
    )
    .map_err(history_error)?;
    checked(read, turn)
}

fn result(
    id: &str,
    thread: &str,
    params: &Params,
    plan: &plan::Plan,
    warnings: &[Value],
    recovered: &[String],
) -> Value {
    json!({"cutexSessionId":id,"threadId":thread,"turnId":params.turn,"dryRun":params.dry,"planDigest":plan.digest,"applicable":plan.applicable(),"files":plan.files,"warnings":warnings,"actionId":null,"applied":false,"journalPath":null,"agentNote":format!("The owner reverted the file changes of turn {} ({} files: {}). History, command effects and subagent changes were not reverted.",params.turn,plan.files.len(),plan.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>().join(", ")),"recovered":recovered})
}

fn history_path_key(cwd: &std::path::Path, raw: &str) -> String {
    let normalized = raw.replace('\\', "/");
    let path = std::path::Path::new(&normalized);
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };
    let path: PathBuf = path.components().collect();
    let key = path.to_string_lossy().replace('\\', "/");
    let key = key.strip_prefix("//?/").unwrap_or(&key);
    if cfg!(windows) {
        key.to_lowercase()
    } else {
        key.to_owned()
    }
}

/// Everything a plan is built from, read from one session snapshot.
struct Prepared {
    changes: Vec<plan::Change>,
    warnings: Vec<Value>,
    root: PathBuf,
    cwd: PathBuf,
    project_root: Option<PathBuf>,
}

impl Prepared {
    fn build(&self, params: &Params) -> Result<plan::Plan, UserInputExecutionError> {
        plan::build_at_with_extra(
            &self.root,
            &self.cwd,
            self.project_root.as_deref(),
            &self.changes,
            params.paths.as_deref(),
        )
        .map_err(|e| error("invalid_request", e, json!({})))
    }
}

fn later_warnings(
    cwd: &std::path::Path,
    changes: &[plan::Change],
    read: &ReadTurn,
    warnings: &mut Vec<Value>,
) {
    let paths: BTreeSet<_> = changes
        .iter()
        .flat_map(|c| std::iter::once(c.path.clone()).chain(c.move_path.clone()))
        .map(|path| history_path_key(cwd, &path))
        .collect();
    let mut later_ids = Vec::new();
    let mut later_paths = BTreeSet::new();
    let mut partial = 0;
    for later in &read.later {
        if !complete(later) {
            partial += 1;
        }
        if let Ok((changes, _)) = plan::changes(later) {
            let touched: Vec<_> = changes
                .iter()
                .flat_map(|c| std::iter::once(c.path.clone()).chain(c.move_path.clone()))
                .filter(|p| paths.contains(&history_path_key(cwd, p)))
                .collect();
            if !touched.is_empty() {
                later_ids.push(later["id"].clone());
                later_paths.extend(touched);
            }
        }
    }
    if !later_ids.is_empty() {
        warnings
            .push(json!({"kind":"laterTurnsTouchedFiles","turnIds":later_ids,"paths":later_paths}));
    }
    if read.unscanned > 0 || partial > 0 {
        warnings.push(json!({"kind":"laterTurnsNotFullyScanned","unscannedTurns":read.unscanned,"partialTurns":partial}));
    }
}

fn prepare(
    context: ManagementRequestContext,
    owner: Option<&HumanManagementPrincipal>,
    id: &str,
    session: &Value,
    params: &Params,
) -> Result<Prepared, UserInputExecutionError> {
    let read = read_turn(context, id, session, &params.turn)?;
    let (changes, mut warnings) =
        plan::changes(&read.turn).map_err(|e| error("turn_history_too_large", e, json!({})))?;
    let cwd = PathBuf::from(
        session["cwd"]
            .as_str()
            .ok_or_else(|| error("cwd_required", "session has no cwd", json!({})))?,
    );
    let root = cwd.clone();
    let mut project_root = None;
    if let (Some(owner), Some(project_id)) = (
        owner,
        session["projectId"]
            .as_str()
            .and_then(|s| crate::agent_management::ProjectId::new(s).ok()),
    ) {
        if let Ok(project) = (context.read_management_project)(owner, &project_id) {
            project_root = project.workspace_root.map(PathBuf::from);
        }
    }
    later_warnings(&cwd, &changes, &read, &mut warnings);
    #[cfg(debug_assertions)]
    if std::env::var_os("CUTEX_TEST_FILES_REVERT_HISTORY").is_some() {
        let private =
            std::env::var_os("CUTEX_TEST_PRIVATE_HOME").and_then(|p| std::fs::canonicalize(p).ok());
        if !private.is_some_and(|p| {
            std::iter::once(&root)
                .chain(project_root.iter())
                .all(|root| std::fs::canonicalize(root).is_ok_and(|r| r.starts_with(&p)))
        }) {
            return Err(error(
                "invalid_request",
                "fixture workspace boundary must be private",
                json!({}),
            ));
        }
    }
    Ok(Prepared {
        changes,
        warnings,
        root,
        cwd,
        project_root,
    })
}

/// Under the execution lock the session is loaded again: a changed native
/// binding refuses, so a plan is never applied to another thread's turn.
fn rebound<'a>(fresh: Option<&'a Value>, thread: &str) -> Result<&'a Value, UserInputExecutionError> {
    let fresh = fresh.ok_or_else(|| {
        error(
            "session_not_found",
            "the session no longer exists",
            json!({}),
        )
    })?;
    let current = fresh.pointer("/native/threadId").and_then(Value::as_str);
    if current != Some(thread) {
        return Err(error(
            "history_changed",
            "the session's native thread binding changed",
            json!({ "threadId": thread, "currentThreadId": current }),
        ));
    }
    if fresh
        .pointer("/runtime/activeTurnId")
        .is_some_and(|v| !v.is_null())
    {
        return Err(error("session_busy", "session became busy", json!({})));
    }
    Ok(fresh)
}

pub(super) fn dispatch(
    repository: &EventRepository,
    context: ManagementRequestContext,
    owner: Option<&HumanManagementPrincipal>,
    id: &str,
    session: &Value,
    request: &ValidatedCutexRequest,
) -> Result<Value, UserInputExecutionError> {
    let (params, thread) = preflight(owner.is_some(), session, request)?;
    if params.dry {
        let prepared = prepare(context, owner, id, session, &params)?;
        let plan = prepared.build(&params)?;
        return Ok(result(id, thread, &params, &plan, &prepared.warnings, &[]));
    }
    let home = crate::config::paths::runtime_dir()
        .map_err(|e| error("persistence_unavailable", e, json!({})))?
        .join("file-reverts");
    let _lock = journal::lock(&home).map_err(|e| error("persistence_unavailable", e, json!({})))?;
    let map_failure = |failure: journal::Failure| {
        let mut e = error(
            failure.code,
            failure.message,
            json!({"journalPath":failure.journal,"outcomeUnknown":failure.unknown}),
        );
        e.outcome_unknown = failure.unknown;
        e
    };
    let recovered = journal::recover(&home).map_err(map_failure)?;
    // Binding, complete history and files are all re-read under the lock; the
    // digest covers the turn's changes and the files' current content.
    let registry = (context.load_registry)()
        .map_err(|e| error("persistence_unavailable", e, json!({})))?;
    let fresh = super::session::session_resource_including_hidden(
        id,
        &registry,
        context.load_runtime_status,
        repository,
    )
    .map_err(|e| error("persistence_unavailable", e, json!({})))?;
    let session = rebound(fresh.as_ref(), thread)?;
    let prepared = prepare(context, owner, id, session, &params)?;
    let plan = prepared.build(&params)?;
    let warnings = &prepared.warnings;
    let mut output = result(id, thread, &params, &plan, warnings, &recovered);
    if params.digest.as_deref() != Some(&plan.digest) {
        output["dryRun"] = json!(true);
        return Err(error("plan_changed", "plan changed since preview", output));
    }
    if plan.blocked() {
        return Err(error(
            "revert_conflict",
            "one or more selected files conflict",
            output,
        ));
    }
    if !plan.applicable() {
        return Err(error(
            "nothing_to_revert",
            "no selected changes remain",
            output,
        ));
    }
    if (context.load_runtime_status)(id)
        .map_err(|e| error("native_history_unavailable", e, json!({})))?
        .is_some_and(|s| s.active_turn_id.is_some())
    {
        return Err(error("session_busy", "session became busy", json!({})));
    }
    let (action, journal) = journal::execute(&home, &plan, params.digest.as_deref().unwrap())
        .map_err(|failure| {
            if failure.code == "plan_changed" {
                match prepared.build(&params) {
                    Ok(fresh) => {
                        let mut details =
                            result(id, thread, &params, &fresh, warnings, &recovered);
                        details["dryRun"] = json!(true);
                        error("plan_changed", failure.message, details)
                    }
                    Err(_) => map_failure(failure),
                }
            } else {
                map_failure(failure)
            }
        })?;
    output["actionId"] = json!(action);
    output["journalPath"] = json!(journal);
    output["applied"] = json!(true);
    append_cutex_event(repository, id, super::model::EventCorrelation { thread_id: Some(thread.into()), management_request_id: Some(request.request_id.clone()), ..Default::default() }, EVENT, json!({"threadId":thread,"turnId":params.turn,"actionId":action,"files":plan.files.iter().filter(|f| f.status == "ok").map(|f| json!({"path":f.path,"action":f.action})).collect::<Vec<_>>(),"revertedAt":chrono::Utc::now().to_rfc3339()})).map_err(super::server::post_operation_persistence_error)?;
    Ok(output)
}

fn preflight<'a>(
    owner: bool,
    session: &'a Value,
    request: &ValidatedCutexRequest,
) -> Result<(Params, &'a str), UserInputExecutionError> {
    if !owner {
        return Err(error(
            "cutex_method_denied",
            "file revert requires the Management root credential",
            json!({}),
        ));
    }
    let params = parse(&request.params)?;
    if request.params.get("expectedRevision").is_some() {
        require_session_revision(session, request.params.as_object().unwrap())?;
    }
    let thread = session
        .pointer("/native/threadId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            error(
                "thread_unbound",
                "session has no bound native thread",
                json!({}),
            )
        })?;
    if !params.dry
        && session
            .pointer("/runtime/activeTurnId")
            .is_some_and(|v| !v.is_null())
    {
        return Err(error(
            "session_busy",
            "session has an active turn",
            json!({}),
        ));
    }
    Ok((params, thread))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_params_and_preview_rejection() {
        for bad in [
            json!({"turnId":"t"}),
            json!({"turnId":"t","dryRun":false}),
            json!({"turnId":"t","dryRun":true,"paths":[1]}),
        ] {
            assert_eq!(parse(&bad).unwrap_err().code, "invalid_request");
        }
        assert!(!complete(&json!({"items":[{"truncated":true}]})));
        assert!(!complete(&json!({"itemsView":"summary"})));
    }
    fn session(connected: bool) -> Value {
        json!({"native":{"threadId":"thread"},"cwd":"/w","runtime":{"appServerConnected":connected,"runtimeGeneration":1}})
    }
    fn sid() -> String {
        format!("cutex.{}", uuid::Uuid::new_v4())
    }
    fn plain(n: usize) -> Value {
        json!({"id":format!("t{n}"),"status":"completed","items":[{"id":format!("t{n}-m"),"type":"agentMessage","text":"x"}]})
    }
    fn file_turn(id: &str, path: &str, diff: &str) -> Value {
        json!({"id":id,"status":"completed","items":[{"id":format!("{id}-f"),"type":"fileChange","status":"completed","changes":[{"path":path,"kind":{"type":"add"},"diff":diff}]}]})
    }
    fn never(_: &str, _: Value) -> Result<Value, HistoryCallError> {
        panic!("this source must not be read")
    }
    fn offline(turns: &[Value], target: &str) -> Result<ReadTurn, UserInputExecutionError> {
        read_turn_with(&sid(), &session(false), target, &mut never, &mut fixture_native("thread", turns))
    }
    #[test]
    fn complete_reader_reads_beyond_500_turns_and_bounds_the_later_scan() {
        let mut turns: Vec<Value> = (0..1200).map(plain).collect();
        turns[0] = file_turn("target", "a.txt", "content\n");
        turns[700] = file_turn("later-hit", "a.txt", "x\n");
        turns[1150] = file_turn("later-hit-2", "A.TXT", "y\n");
        let read = offline(&turns, "target").unwrap();
        assert_eq!(read.turn["items"][0]["changes"][0]["diff"], "content\n");
        assert_eq!(read.turn["itemsView"], "full");
        assert_eq!(read.later.len(), LATER_SCAN_LIMIT);
        assert_eq!(read.unscanned, 1199 - LATER_SCAN_LIMIT);
        assert_eq!(read.later[0]["id"], "later-hit");
        assert_eq!(read.later[LATER_SCAN_LIMIT - 1]["id"], "t1199");
        let (changes, mut warnings) = plan::changes(&read.turn).unwrap();
        later_warnings(std::path::Path::new("/w"), &changes, &read, &mut warnings);
        assert_eq!(warnings[0]["kind"], "laterTurnsTouchedFiles");
        let hits: Vec<_> = warnings[0]["turnIds"].as_array().unwrap().iter().collect();
        assert_eq!(hits.len(), if cfg!(windows) { 2 } else { 1 });
        assert_eq!(warnings[1], json!({"kind":"laterTurnsNotFullyScanned","unscannedTurns":699,"partialTurns":0}));
        let read = offline(&turns, "t1100").unwrap();
        assert_eq!(read.later.len(), 99);
        assert_eq!(read.unscanned, 0);
        assert_eq!(read.later[0]["id"], "t1101");
        let read = offline(&turns, "t1199").unwrap();
        assert!(read.later.is_empty());
        assert_eq!(read.unscanned, 0);
    }
    #[test]
    fn incomplete_or_truncated_turns_are_refused_never_previewed() {
        // Above the 512 KiB HTTP preview budget, below the complete limit: whole.
        let big = "b".repeat(history::PREVIEW_BYTES * 2);
        let read = offline(&[file_turn("t", "a.txt", &big), plain(1)], "t").unwrap();
        assert_eq!(read.turn["items"][0]["changes"][0]["diff"].as_str().unwrap().len(), big.len());
        assert!(read.turn.get("truncated").is_none());
        // Over the complete limit: refused, not truncated.
        let huge = "h".repeat(17 * 1024 * 1024);
        let err = offline(&[file_turn("t", "a.txt", &huge)], "t").unwrap_err();
        assert_eq!(err.code, "turn_history_too_large");
        // Native truncation markers anywhere in an item: refused.
        let mut marked = file_turn("t", "a.txt", "c\n");
        marked["items"][0]["changes"][0]["truncated"] = json!(true);
        assert_eq!(offline(&[marked], "t").unwrap_err().code, "turn_history_too_large");
        let mut summary = file_turn("t", "a.txt", "c\n");
        summary["items"][0]["itemsView"] = json!("summary");
        assert_eq!(offline(&[summary], "t").unwrap_err().code, "turn_history_too_large");
        // A native "too large" answer for the items: refused.
        let turns = [file_turn("t", "a.txt", "c\n")];
        let mut fixture = fixture_native("thread", &turns);
        let mut native = |method: &str, params: Value| {
            if method == "thread/items/list" {
                return Err(HistoryCallError::Rpc(json!({"code":-32001,"message":"too large"})));
            }
            fixture(method, params)
        };
        let err = read_turn_with(&sid(), &session(false), "t", &mut never, &mut native).unwrap_err();
        assert_eq!(err.code, "turn_history_too_large");
        assert_eq!(super::super::server::cutex_operation_error_status(&err.code, err.outcome_unknown), 422);
    }
    #[test]
    fn running_missing_and_partial_later_turns() {
        let running = json!({"id":"t","status":"inProgress","items":[]});
        assert_eq!(offline(&[running], "t").unwrap_err().code, "turn_in_progress");
        assert_eq!(offline(&[plain(0)], "missing").unwrap_err().code, "turn_not_found");
        let mut later = file_turn("later", "a.txt", "x\n");
        later["items"][0]["truncated"] = json!(true);
        let read = offline(&[file_turn("t", "a.txt", "c\n"), later], "t").unwrap();
        let (changes, mut warnings) = plan::changes(&read.turn).unwrap();
        later_warnings(std::path::Path::new("/w"), &changes, &read, &mut warnings);
        assert_eq!(warnings.last().unwrap(), &json!({"kind":"laterTurnsNotFullyScanned","unscannedTurns":0,"partialTurns":1}));
    }
    #[test]
    fn live_source_falls_back_only_when_unavailable() {
        let turns = [file_turn("t", "a.txt", "c\n")];
        let mut gone = |_: &str, _: Value| -> Result<Value, HistoryCallError> { Err(HistoryCallError::Unavailable("gone".into())) };
        let read = read_turn_with(&sid(), &session(true), "t", &mut gone, &mut fixture_native("thread", &turns)).unwrap();
        assert_eq!(read.turn["id"], "t");
        let mut broken = |_: &str, _: Value| -> Result<Value, HistoryCallError> { Err(HistoryCallError::Rpc(json!({"code":-32603,"message":"bad"}))) };
        let err = read_turn_with(&sid(), &session(true), "t", &mut broken, &mut never).unwrap_err();
        assert_eq!(err.code, "native_history_unavailable");
        let mut gone = |_: &str, _: Value| -> Result<Value, HistoryCallError> { Err(HistoryCallError::Unavailable("gone".into())) };
        let mut also_gone = |_: &str, _: Value| -> Result<Value, HistoryCallError> { Err(HistoryCallError::Unavailable("gone".into())) };
        let err = read_turn_with(&sid(), &session(true), "t", &mut gone, &mut also_gone).unwrap_err();
        assert_eq!(err.code, "codex_app_server_unavailable");
        assert!(err.retryable);
        let err = read_turn_with(&sid(), &session(false), "t", &mut never, &mut fixture_native("other-thread", &turns)).unwrap_err();
        assert_eq!(err.code, "native_history_unavailable");
    }
    #[test]
    fn execution_rechecks_binding_and_busy_under_the_lock() {
        assert_eq!(rebound(None, "thread").unwrap_err().code, "session_not_found");
        let rebound_session = json!({"native":{"threadId":"other"},"runtime":{}});
        let err = rebound(Some(&rebound_session), "thread").unwrap_err();
        assert_eq!(err.code, "history_changed");
        assert_eq!(super::super::server::cutex_operation_error_status(&err.code, false), 409);
        let busy = json!({"native":{"threadId":"thread"},"runtime":{"activeTurnId":"t2"}});
        assert_eq!(rebound(Some(&busy), "thread").unwrap_err().code, "session_busy");
        let idle = json!({"native":{"threadId":"thread"},"runtime":{"activeTurnId":null}});
        assert!(rebound(Some(&idle), "thread").is_ok());
    }
    #[test]
    fn root_revision_binding_and_busy_are_checked_before_io() {
        let mut request = ValidatedCutexRequest {
            request_id: "r".into(),
            method: METHOD.into(),
            params: json!({"turnId":"t","dryRun":false,"expectedPlanDigest":"sha256:test"}),
        };
        let busy = json!({"revision":4,"native":{"threadId":"thread"},"runtime":{"activeTurnId":"running"}});
        assert_eq!(
            preflight(false, &busy, &request).unwrap_err().code,
            "cutex_method_denied"
        );
        assert_eq!(
            preflight(true, &json!({}), &request).unwrap_err().code,
            "thread_unbound"
        );
        assert_eq!(
            preflight(true, &busy, &request).unwrap_err().code,
            "session_busy"
        );
        request.params["dryRun"] = json!(true);
        assert!(preflight(true, &busy, &request).is_ok());
        request.params["expectedRevision"] = json!(3);
        assert_eq!(
            preflight(true, &busy, &request).unwrap_err().code,
            "revision_conflict"
        );
    }
    #[test]
    fn schema_accepts_results_and_requires_execute_digest() {
        let plan = plan::Plan {
            root: PathBuf::from("/private"),
            extra_root: None,
            files: vec![],
            writes: vec![],
            digest: format!("sha256:{}", "a".repeat(64)),
        };
        let params = parse(&json!({"turnId":"t","dryRun":true})).unwrap();
        let warnings = [json!({"kind":"laterTurnsNotFullyScanned","unscannedTurns":3,"partialTurns":0})];
        let output = result("cutex.s", "thread", &params, &plan, &warnings, &[]);
        super::super::contract_validation::validate_cutex_response(&json!({"contractVersion":2,"requestId":"r","cutexSessionId":"cutex.s","cutex":{"method":METHOD,"result":output}})).unwrap();
        assert!(super::super::contract_validation::validate_cutex_request(
            &json!({"requestId":"r","method":METHOD,"params":{"turnId":"t","dryRun":false}})
        )
        .is_err());
        super::super::contract_validation::validate_cutex_event_message(&json!({"method":EVENT,"params":{"threadId":"thread","turnId":"t","actionId":"frv-test","files":[{"path":"a","action":"restore"}],"revertedAt":"2026-10-11T00:00:00Z"}})).unwrap();
        assert_eq!(
            super::super::server::cutex_operation_error_status("revert_partial", true),
            500
        );
    }
}
