//! `cutex human session delete <id> [--yes] [--json]`: permanently delete a
//! cutex session through the running Management API, exactly like the
//! desktop's "Delete…" (Management `cutex/session/delete`, app layer in
//! [`super::session_delete`]).
//!
//! The target is resolved locally (cutex_session_id, Codex session id or
//! unique display / thread name, archived sessions included) so the owner
//! confirms what is deleted; the request then carries that record's durable
//! revision (`expectedRevision`), so a session that changed in between is not
//! deleted. The command authenticates as the owner with the local
//! Management root credential (never printed).

use std::fmt;
use std::io::{BufRead, IsTerminal, Write};
use std::time::Duration;

use serde_json::{json, Value};

use cutex::session::model::CutexSessionRecord;

/// Stops a runtime, may start a private native and copy histories.
const DELETE_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Debug)]
pub(crate) struct DeleteCommandError {
    code: String,
    message: String,
    details: Value,
    outcome_unknown: bool,
}

impl DeleteCommandError {
    fn new(code: &str, message: impl Into<String>, details: Value) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details,
            outcome_unknown: false,
        }
    }

    fn json_envelope(&self) -> Value {
        json!({
            "stage": "delete",
            "code": self.code,
            "message": self.message,
            "details": self.details,
            "outcomeUnknown": self.outcome_unknown,
        })
    }
}

impl fmt::Display for DeleteCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for DeleteCommandError {}

/// With `--json` the error is one JSON object on stderr (see `main`).
#[derive(Debug)]
struct DeleteJsonProcessError(DeleteCommandError);

impl fmt::Display for DeleteJsonProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for DeleteJsonProcessError {}

pub(crate) fn json_process_error(error: &anyhow::Error) -> Option<Value> {
    error
        .downcast_ref::<DeleteJsonProcessError>()
        .map(|error| error.0.json_envelope())
}

pub(crate) fn cmd_session_delete(id: &str, yes: bool, json_output: bool) -> anyhow::Result<()> {
    let wrap = |error: DeleteCommandError| {
        if json_output {
            anyhow::Error::new(DeleteJsonProcessError(error))
        } else {
            anyhow::Error::new(error)
        }
    };
    let record = resolve_target(id).map_err(wrap)?;
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let stdin = std::io::stdin();
    confirm(
        &record,
        yes,
        interactive,
        &mut stdin.lock(),
        &mut std::io::stderr(),
    )
    .map_err(wrap)?;
    let client = super::management_control_plane::ManagementControlClient::connect()
        .map_err(|error| {
            wrap(DeleteCommandError::new(
                "management_unavailable",
                format!(
                    "the Management API is needed to delete a session (nothing was deleted): {error:#}"
                ),
                json!({ "cutexSessionId": record.cutex_session_id }),
            ))
        })?;
    let receipt = delete_with_client(&client, &record).map_err(wrap)?;
    present(&record, &receipt, json_output)
}

/// The record `id` names on this host's store (archived included).
fn resolve_target(id: &str) -> Result<CutexSessionRecord, DeleteCommandError> {
    let store = cutex::session::store::load_cutex_session_store_shared().map_err(|error| {
        DeleteCommandError::new(
            "session_lookup_failed",
            format!("failed to load durable Cutex sessions: {error:#}"),
            json!({ "id": id }),
        )
    })?;
    resolve_in(&store, id, &cutex::platform::host::current_host_name())
}

fn resolve_in(
    store: &cutex::session::model::CutexSessionStore,
    id: &str,
    host: &str,
) -> Result<CutexSessionRecord, DeleteCommandError> {
    let key = cutex::session::service::cutex_session_key_for_user_id_including_retired(store, id)
        .ok_or_else(|| {
            DeleteCommandError::new(
                "session_not_found",
                format!(
                    "cutex session is not known (or the name is not unique): {id}; use the cutex_session_id or Codex session id"
                ),
                json!({ "id": id }),
            )
        })?;
    let record = store.sessions.get(&key).cloned().ok_or_else(|| {
        DeleteCommandError::new(
            "session_not_found",
            format!("cutex session disappeared while loading: {key}"),
            json!({ "id": id }),
        )
    })?;
    if !cutex::runtime::lifecycle::cutex_session_host_is_local(&record.host_id, host) {
        return Err(DeleteCommandError::new(
            "session_delete_unsupported",
            format!(
                "{key} belongs to host {}; delete it on that host",
                record.host_id
            ),
            json!({ "cutexSessionId": key, "hostId": record.host_id }),
        ));
    }
    if record.registration_class == cutex::agent_bus::model::AgentRegistrationClass::Ephemeral {
        return Err(DeleteCommandError::new(
            "session_delete_unsupported",
            format!("{key} is an ephemeral runtime record, not a durable session"),
            json!({ "cutexSessionId": key }),
        ));
    }
    Ok(record)
}

fn kind_label(record: &CutexSessionRecord) -> &'static str {
    match (record.claude_session.is_some(), record.agent_enabled) {
        (true, true) => "Claude agent",
        (true, false) => "Claude session",
        (false, true) => "agent",
        (false, false) => "local session",
    }
}

fn describe(record: &CutexSessionRecord) -> String {
    let mut lines = vec![format!(
        "{} \"{}\" ({})",
        kind_label(record),
        cutex::session::service::cutex_session_display_name(record),
        record.cutex_session_id
    )];
    if let Some(thread) = record.app_server_thread_id() {
        lines.push(format!("  native thread: {thread}"));
    }
    if let Some(contract) = &record.explicit_launch {
        lines.push(format!(
            "  native home:   {}",
            cutex::ui::format::compact_home_path(&contract.native_home.to_string_lossy())
        ));
    }
    lines.push(format!("  cwd:           {}", record.cwd));
    if record.is_retired() {
        lines.push("  state:         archived".to_string());
    }
    lines.join("\n")
}

/// `--yes` skips the question; otherwise a TTY must confirm, and a
/// non-interactive caller is refused before anything is sent.
fn confirm(
    record: &CutexSessionRecord,
    yes: bool,
    interactive: bool,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> Result<(), DeleteCommandError> {
    if yes {
        return Ok(());
    }
    let details = json!({ "cutexSessionId": record.cutex_session_id });
    if !interactive {
        return Err(DeleteCommandError::new(
            "confirmation_required",
            "permanent delete needs confirmation: run it on a terminal or pass --yes",
            details,
        ));
    }
    let io_error = |error: std::io::Error| {
        DeleteCommandError::new(
            "confirmation_required",
            format!("could not read the confirmation: {error}"),
            details.clone(),
        )
    };
    write!(
        output,
        "{}\nPermanently delete this session? Its record and native history are removed; a recovery copy is kept under runtime/session-deletes/. [y/N] ",
        describe(record)
    )
    .and_then(|_| output.flush())
    .map_err(io_error)?;
    let mut line = String::new();
    input.read_line(&mut line).map_err(io_error)?;
    let answer = super::prompt::normalize_prompt_input(&line).to_ascii_lowercase();
    if matches!(answer.as_str(), "y" | "yes") {
        Ok(())
    } else {
        Err(DeleteCommandError::new(
            "delete_cancelled",
            "not confirmed; nothing was deleted",
            details,
        ))
    }
}

fn delete_with_client(
    client: &super::management_control_plane::ManagementControlClient,
    record: &CutexSessionRecord,
) -> Result<Value, DeleteCommandError> {
    let id = record.cutex_session_id.as_str();
    let request_id = format!("cli-delete-{}", uuid::Uuid::new_v4());
    let method = cutex::management::v2::session_delete::METHOD;
    let body = json!({
        "requestId": request_id,
        "method": method,
        "params": { "expectedRevision": record.durable_revision() },
    });
    let encoded: String = url::form_urlencoded::byte_serialize(id.as_bytes()).collect();
    let response: Value = client
        .request_with_timeout(
            "POST",
            &format!("/v2/sessions/{encoded}/cutex/requests"),
            Some(&serde_json::to_vec(&body).expect("delete request serializes")),
            DELETE_TIMEOUT,
        )
        .map_err(|error| delete_error(error, id))?;
    let result = &response["cutex"]["result"];
    let matches = response["requestId"] == request_id.as_str()
        && response["cutexSessionId"] == id
        && response["cutex"]["method"] == method
        && result["deleted"] == true
        && result["cutexSessionId"] == id;
    if !matches {
        return Err(DeleteCommandError {
            outcome_unknown: true,
            ..DeleteCommandError::new(
                "delete_response_unconfirmed",
                format!(
                    "the Management response does not confirm the delete of {id}; check `cutex human session show {id}` and runtime/session-deletes/ before retrying"
                ),
                json!({ "cutexSessionId": id, "response": response }),
            )
        });
    }
    Ok(result.clone())
}

fn delete_error(error: anyhow::Error, id: &str) -> DeleteCommandError {
    let details = json!({ "cutexSessionId": id });
    match error.downcast_ref::<cutex::http::client::HttpStatusError>() {
        Some(status) => {
            let code = status.code.clone().unwrap_or_else(|| "delete_failed".into());
            let message = status.message.clone().unwrap_or_else(|| status.to_string());
            let hint = match code.as_str() {
                "session_referenced" => "; nothing was deleted",
                "session_busy" => "; nothing was deleted, retry when it is idle",
                "revision_conflict" => "; the session changed meanwhile, nothing was deleted",
                _ if status.status == 404 => "; the Management API does not know this session",
                _ => "",
            };
            let outcome_unknown = matches!(code.as_str(), "persistence_uncertain" | "outcome_unknown");
            DeleteCommandError {
                outcome_unknown,
                ..DeleteCommandError::new(
                    &code,
                    format!("{message}{hint}"),
                    json!({ "cutexSessionId": id, "httpStatus": status.status }),
                )
            }
        }
        None if format!("{error:#}").contains("Failed to connect cutex management API") => {
            DeleteCommandError::new(
                "management_unavailable",
                format!("{error:#}; nothing was deleted"),
                details,
            )
        }
        None => DeleteCommandError {
            outcome_unknown: true,
            ..DeleteCommandError::new(
                "delete_response_unconfirmed",
                format!(
                    "{error:#}; the delete may or may not have happened: check `cutex human session show {id}` and runtime/session-deletes/ before retrying"
                ),
                details,
            )
        },
    }
}

fn present(record: &CutexSessionRecord, receipt: &Value, json_output: bool) -> anyhow::Result<()> {
    if json_output {
        println!("{}", serde_json::to_string_pretty(receipt)?);
        return Ok(());
    }
    let native = match receipt["nativeDelete"].as_str() {
        Some("thread/delete") => "native thread deleted",
        Some("storageMove") => "native history moved into the recovery directory",
        Some("claudeState") => "Claude session state moved into the recovery directory",
        Some("dshState") => "DSH agent state moved into the recovery directory",
        _ => "no native history to delete",
    };
    println!(
        "Deleted {} \"{}\" ({})",
        kind_label(record),
        cutex::session::service::cutex_session_display_name(record),
        receipt["cutexSessionId"].as_str().unwrap_or(&record.cutex_session_id)
    );
    if let Some(thread) = receipt["threadId"].as_str() {
        println!("  native thread: {thread} ({native})");
    } else {
        println!("  {native}");
    }
    if receipt["runtimeStopped"] == true {
        println!("  its running runtime was stopped first");
    }
    println!(
        "  recovery copy: {}",
        receipt["backupPath"].as_str().unwrap_or("-")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cutex::agent_bus::model::AgentRegistrationClass;
    use cutex::session::model::CutexSessionStore;

    const THREAD: &str = "018a0000-0000-7000-8000-0000000000e1";

    fn record(id: &str, host: &str) -> CutexSessionRecord {
        let mut record = CutexSessionRecord::new_at(
            id.into(),
            Some(THREAD.into()),
            host.into(),
            "/work".into(),
            None,
            "2026-10-01T00:00:00Z".into(),
        )
        .unwrap();
        record.display_name_hint = Some("Quick chat".into());
        record.registration_class = AgentRegistrationClass::LocalOnly;
        record
    }

    fn store(records: Vec<CutexSessionRecord>) -> CutexSessionStore {
        let mut store = CutexSessionStore::default();
        for record in records {
            store.sessions.insert(record.cutex_session_id.clone(), record);
        }
        store
    }

    #[test]
    fn target_resolves_by_id_thread_or_unique_name_on_this_host_only() {
        let store = store(vec![record("cutex.a", "here")]);
        for id in ["cutex.a", THREAD, "Quick chat"] {
            assert_eq!(resolve_in(&store, id, "here").unwrap().cutex_session_id, "cutex.a", "{id}");
        }
        assert_eq!(resolve_in(&store, "nope", "here").unwrap_err().code, "session_not_found");
        assert_eq!(
            resolve_in(&store, "cutex.a", "elsewhere").unwrap_err().code,
            "session_delete_unsupported"
        );
        let mut twin = record("cutex.b", "here");
        twin.codex_session_id = None;
        let ambiguous = self::store(vec![record("cutex.a", "here"), twin]);
        assert_eq!(resolve_in(&ambiguous, "Quick chat", "here").unwrap_err().code, "session_not_found");
        let mut ephemeral = record("cutex.e", "here");
        ephemeral.registration_class = AgentRegistrationClass::Ephemeral;
        let store = self::store(vec![ephemeral]);
        assert_eq!(resolve_in(&store, "cutex.e", "here").unwrap_err().code, "session_delete_unsupported");
    }

    #[test]
    fn confirmation_needs_yes_or_an_interactive_y() {
        let target = record("cutex.a", "here");
        let mut out = Vec::new();
        assert!(confirm(&target, true, false, &mut &b""[..], &mut out).is_ok());
        assert!(out.is_empty(), "--yes asks nothing");
        let refused = confirm(&target, false, false, &mut &b"y\n"[..], &mut out).unwrap_err();
        assert_eq!(refused.code, "confirmation_required");
        assert!(out.is_empty(), "non-interactive never prompts");
        assert!(confirm(&target, false, true, &mut &b"y\n"[..], &mut out).is_ok());
        let prompt = String::from_utf8(out).unwrap();
        assert!(prompt.contains("cutex.a") && prompt.contains("[y/N]"), "{prompt}");
        for answer in [&b"\n"[..], b"n\n", b"", b"delete it\n"] {
            let mut out = Vec::new();
            assert_eq!(
                confirm(&target, false, true, &mut &answer[..], &mut out).unwrap_err().code,
                "delete_cancelled"
            );
        }
    }

    /// One HTTP exchange: returns the request body the server saw.
    fn serve_once(status: &'static str, response: Value) -> (String, std::thread::JoinHandle<Value>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut bytes = Vec::new();
            let (head, body) = loop {
                let mut buf = [0u8; 4096];
                let n = socket.read(&mut buf).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buf[..n]);
                if let Some(split) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8(bytes[..split].to_vec()).unwrap();
                    let length: usize = headers
                        .lines()
                        .find_map(|l| l.strip_prefix("Content-Length: "))
                        .unwrap()
                        .parse()
                        .unwrap();
                    if bytes.len() >= split + 4 + length {
                        break (headers, bytes[split + 4..split + 4 + length].to_vec());
                    }
                }
            };
            let mut seen: Value = serde_json::from_slice(&body).unwrap();
            seen["_requestLine"] = json!(head.lines().next().unwrap());
            seen["_auth"] = json!(head.lines().any(|l| l == "Authorization: Bearer test-root"));
            let reply = if response.is_null() {
                json!({
                    "contractVersion": 2, "requestId": seen["requestId"], "cutexSessionId": "cutex.a",
                    "cutex": { "method": "cutex/session/delete", "result": {
                        "cutexSessionId": "cutex.a", "deleted": true, "threadId": THREAD,
                        "nativeDelete": "storageMove", "runtimeStopped": false,
                        "backupPath": "/home/x/.cutex/runtime/session-deletes/20261020T000000.000Z-cutex.a",
                        "deletedAt": "2026-10-20T00:00:00Z" } }
                })
            } else {
                response
            }
            .to_string();
            write!(
                socket,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            )
            .unwrap();
            seen
        });
        (base, server)
    }

    #[test]
    fn delete_posts_the_cutex_method_with_the_confirmed_revision() {
        let mut target = record("cutex.a", "here");
        target.revision = 7;
        let (base, server) = serve_once("200 OK", Value::Null);
        let client = super::super::management_control_plane::ManagementControlClient::test_endpoint(base, "test-root".into());
        let receipt = delete_with_client(&client, &target).unwrap();
        assert_eq!(receipt["nativeDelete"], "storageMove");
        let seen = server.join().unwrap();
        assert_eq!(seen["_requestLine"], "POST /v2/sessions/cutex.a/cutex/requests HTTP/1.1");
        assert_eq!(seen["_auth"], true);
        assert_eq!(seen["method"], "cutex/session/delete");
        assert_eq!(seen["params"], json!({ "expectedRevision": target.durable_revision() }));
    }

    #[test]
    fn refusals_keep_their_code_and_say_nothing_was_deleted() {
        let target = record("cutex.a", "here");
        let (base, server) = serve_once(
            "409 Conflict",
            json!({ "error": { "code": "session_referenced", "message": "this session is still referenced" } }),
        );
        let client = super::super::management_control_plane::ManagementControlClient::test_endpoint(base, "test-root".into());
        let error = delete_with_client(&client, &target).unwrap_err();
        server.join().unwrap();
        assert_eq!(error.code, "session_referenced");
        assert!(error.message.contains("nothing was deleted"), "{}", error.message);
        assert!(!error.outcome_unknown);
        let envelope = json_process_error(&anyhow::Error::new(DeleteJsonProcessError(error))).unwrap();
        assert_eq!(envelope["code"], "session_referenced");
    }
}
