//! Generated titles for quick chats (settings `autoThreadTitles`,
//! `threadTitleModel`).
//!
//! The TUI names a thread after its first message with a hidden ephemeral
//! structured turn; app-server clients (the desktop) never did, so quick
//! chats stayed 「新会话」. Cutex does the same here, on the session's own
//! app-server: after the first completed user message of an owned session
//! with no thread name and no owner-set name, it starts an ephemeral
//! read-only thread with every tool, MCP server and feature off, runs one
//! turn with a `{title}` output schema (prompt ≤ 960 bytes: the TUI's
//! instructions + that message), and saves the result with `thread/name/set`;
//! the native `thread/name/updated` then updates the record as usual.
//!
//! Model: the setting, else `gpt-5.6-luna` (low effort) on a ChatGPT login,
//! else the session's own default. Claude sessions are skipped (their adapter
//! has no ephemeral threads). One attempt per session per daemon run; any
//! failure is logged and leaves the name unset.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use cutex::app_server::client::AppServerEvent;
use cutex::app_server::manager::AppServerRuntimeEventContext;
use serde_json::json;
use serde_json::Value;

pub(crate) const TITLE_MAX_CHARS: usize = 36;
const TITLE_MODEL: &str = "gpt-5.6-luna";
const PROMPT_MAX_BYTES: usize = 960;
const TURN_TIMEOUT: Duration = Duration::from_secs(30);
const RESPONSE_MAX_BYTES: usize = 8 * 1024;

/// Sessions with an attempt this daemon run.
static ATTEMPTED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
/// Hidden title threads → their event receiver.
static TITLE_THREADS: Mutex<Option<HashMap<String, mpsc::Sender<Value>>>> = Mutex::new(None);

/// Native thread id an event belongs to (`threadId`, `thread.id` or
/// `turn.threadId` in its params).
fn event_thread_id(params: &Value) -> Option<&str> {
    params
        .get("threadId")
        .or_else(|| params.pointer("/thread/id"))
        .or_else(|| params.pointer("/turn/threadId"))
        .and_then(Value::as_str)
}

/// Swallow events of a hidden title thread (they never reach the session's
/// event stream); returns true when the event was one.
pub(super) fn claim_event(event: &AppServerEvent) -> bool {
    let (method, params) = match event {
        AppServerEvent::Notification(n) => (n.method.as_str(), n.params.as_ref()),
        AppServerEvent::ServerRequest(r) => (r.method.as_str(), r.params.as_ref()),
        _ => return false,
    };
    let Some(thread_id) = params.and_then(event_thread_id) else {
        return false;
    };
    let Ok(guard) = TITLE_THREADS.lock() else {
        return false;
    };
    let Some(sender) = guard.as_ref().and_then(|threads| threads.get(thread_id)) else {
        return false;
    };
    let _ = sender.send(json!({ "method": method, "params": params }));
    true
}

/// Text of a completed `userMessage` item on the session's own thread.
fn first_user_text(context: &AppServerRuntimeEventContext, event: &AppServerEvent) -> Option<String> {
    let AppServerEvent::Notification(notification) = event else {
        return None;
    };
    if notification.method != "item/completed" {
        return None;
    }
    let params = notification.params.as_ref()?;
    if params.get("threadId").and_then(Value::as_str) != Some(context.thread_id.as_str()) {
        return None;
    }
    let item = params.get("item")?;
    if item.get("type").and_then(Value::as_str) != Some("userMessage") {
        return None;
    }
    let text = item
        .get("content")?
        .as_array()?
        .iter()
        .filter(|input| input.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|input| input.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    (!text.trim().is_empty()).then_some(text)
}

/// Runtime hook: start a title for an unnamed quick chat's first message.
pub(super) fn on_runtime_event(context: &AppServerRuntimeEventContext, event: &AppServerEvent) {
    let Some(text) = first_user_text(context, event) else {
        return;
    };
    let config = cutex::config::store::load_codez_config();
    if !config.auto_thread_titles {
        return;
    }
    let Ok(Some(record)) = session_record(&context.cutex_session_id) else {
        return;
    };
    if !needs_title(&record) {
        return;
    }
    {
        let Ok(mut attempted) = ATTEMPTED.lock() else {
            return;
        };
        if !attempted.get_or_insert_with(HashSet::new).insert(context.cutex_session_id.clone()) {
            return;
        }
    }
    let session = context.cutex_session_id.clone();
    let thread_id = context.thread_id.clone();
    let profile = context.current_profile().name;
    let cwd = record.cwd.clone();
    let model = config.thread_title_model.clone();
    let session_model = record.model_defaults.clone();
    let _ = std::thread::Builder::new()
        .name("cutex-thread-title".to_string())
        .spawn(move || {
            let (model, effort) = title_model(model, profile.as_deref(), session_model);
            if let Err(error) = generate(&session, &thread_id, &cwd, model, effort, &text) {
                eprintln!("\x1b[33mwarning:\x1b[0m thread title for {session}: {error:#}");
            }
        });
}

fn session_record(cutex_session_id: &str) -> anyhow::Result<Option<cutex::session::model::CutexSessionRecord>> {
    let store = cutex::session::store::load_cutex_session_store()?;
    Ok(store
        .sessions
        .values()
        .find(|record| record.cutex_session_id == cutex_session_id)
        .cloned())
}

fn needs_title(record: &cutex::session::model::CutexSessionRecord) -> bool {
    record.is_owned_session()
        && record.claude_session.is_none()
        && !record.is_retired()
        && record.display_name_override.is_none()
        && record.thread_name.as_deref().is_none_or(|name| name.trim().is_empty())
}

/// The setting, else `gpt-5.6-luna` (low) for a ChatGPT login, else the
/// session's own model (none: the app-server default).
fn title_model(
    setting: Option<String>,
    profile: Option<&str>,
    session_model: Option<String>,
) -> (Option<String>, Option<&'static str>) {
    if let Some(model) = setting.filter(|m| !m.trim().is_empty()) {
        let effort = (model == TITLE_MODEL).then_some("low");
        return (Some(model), effort);
    }
    let chatgpt = profile
        .and_then(|name| cutex::profiles::quota::find_profile(name).ok().flatten())
        .is_some_and(|account| {
            cutex::management::v2::profile_quota_native::openai_codex_profile(&account)
                && cutex::management::v2::profile_quota_native::chatgpt_logged_in(&account)
        });
    if chatgpt {
        (Some(TITLE_MODEL.to_string()), Some("low"))
    } else {
        (session_model.filter(|m| !m.trim().is_empty()), None)
    }
}

fn generate(
    session: &str,
    thread_id: &str,
    cwd: &str,
    model: Option<String>,
    effort: Option<&str>,
    user_text: &str,
) -> anyhow::Result<()> {
    let handle = super::app_server_runtime::runtime_manager().handle(session)?;
    let (title_thread, receiver) = start_thread(&handle, cwd, model)?;
    let result = run_turn(&handle, &title_thread, &receiver, &title_prompt(user_text), effort);
    let _ = handle.request("thread/unsubscribe", json!({ "threadId": title_thread }));
    if let Ok(mut guard) = TITLE_THREADS.lock() {
        if let Some(threads) = guard.as_mut() {
            threads.remove(&title_thread);
        }
    }
    let Some(title) = parse_title(&result?) else {
        anyhow::bail!("the title turn returned no usable title");
    };
    // The owner (or the native) may have named it meanwhile.
    if !session_record(session)?.is_some_and(|record| needs_title(&record)) {
        return Ok(());
    }
    handle
        .request("thread/name/set", json!({ "threadId": thread_id, "name": title }))
        .map_err(|error| anyhow::anyhow!("thread/name/set: {error}"))?;
    Ok(())
}

/// Every feature, tool and MCP server off; read-only; ephemeral.
fn start_thread(
    handle: &cutex::app_server::client::AppServerHandle,
    cwd: &str,
    model: Option<String>,
) -> anyhow::Result<(String, mpsc::Receiver<Value>)> {
    let mut config = serde_json::Map::new();
    for feature in [
        "apps", "code_mode", "code_mode_only", "context_management", "current_time_reminder",
        "deferred_executor", "enable_fanout", "goals", "hooks", "image_generation", "memories",
        "multi_agent", "multi_agent_v2", "plugins", "request_permissions_tool", "shell_snapshot",
        "shell_tool", "standalone_web_search", "token_budget", "tool_suggest", "unified_exec",
        "view_image",
    ] {
        config.insert(format!("features.{feature}"), json!(false));
    }
    for key in [
        "cloud.skills.enabled",
        "skills.include_instructions",
        "tools.experimental_request_user_input.enabled",
        "tools.update_plan.enabled",
    ] {
        config.insert(key.to_string(), json!(false));
    }
    config.insert("web_search".to_string(), json!("disabled"));
    config.insert("default_permissions".to_string(), json!(":read-only"));
    // Fail closed (like the TUI): an unreadable config could hide MCP servers.
    let effective = handle
        .request("config/read", json!({ "includeLayers": false, "cwd": cwd }))
        .map_err(|error| anyhow::anyhow!("config/read: {error}"))?;
    // One dotted key per server: a whole `mcp_servers` table would replace
    // the configured one and drop each server's transport (invalid config).
    let servers = effective
        .pointer("/config/mcp_servers")
        .and_then(Value::as_object)
        .map(|servers| servers.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    for name in servers {
        anyhow::ensure!(!name.contains('.'), "MCP server name {name:?} cannot be disabled by key");
        config.insert(format!("mcp_servers.{name}.enabled"), json!(false));
    }
    let mut params = json!({
        "cwd": cwd,
        "approvalPolicy": "never",
        "sandbox": "read-only",
        "runtimeWorkspaceRoots": [],
        "ephemeral": true,
        "environments": [],
        "dynamicTools": [],
        "selectedCapabilityRoots": [],
        "config": config,
    });
    if let Some(model) = model {
        params["model"] = json!(model);
    }
    // Register before the response so no event of the new thread can leak:
    // the thread id is only known from the response, so events that arrive
    // first are buffered by the lineage resolver and then dropped.
    let started = handle
        .request("thread/start", params)
        .map_err(|error| anyhow::anyhow!("thread/start: {error}"))?;
    let title_thread = started
        .pointer("/thread/id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("thread/start returned no thread id"))?
        .to_string();
    anyhow::ensure!(
        started.pointer("/sandbox/type").and_then(Value::as_str) == Some("readOnly"),
        "the title thread did not start read-only"
    );
    let (sender, receiver) = mpsc::channel();
    TITLE_THREADS
        .lock()
        .map_err(|_| anyhow::anyhow!("title registry poisoned"))?
        .get_or_insert_with(HashMap::new)
        .insert(title_thread.clone(), sender);
    Ok((title_thread, receiver))
}

fn run_turn(
    handle: &cutex::app_server::client::AppServerHandle,
    title_thread: &str,
    receiver: &mpsc::Receiver<Value>,
    prompt: &str,
    effort: Option<&str>,
) -> anyhow::Result<String> {
    let mut params = json!({
        "threadId": title_thread,
        "input": [{ "type": "text", "text": prompt, "text_elements": [] }],
        "outputSchema": output_schema(),
    });
    if let Some(effort) = effort {
        params["effort"] = json!(effort);
    }
    let started = handle
        .request("turn/start", params)
        .map_err(|error| anyhow::anyhow!("turn/start: {error}"))?;
    let turn_id = started
        .pointer("/turn/id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("turn/start returned no turn id"))?
        .to_string();
    collect_response(receiver, &turn_id, Instant::now() + TURN_TIMEOUT)
}

fn collect_response(receiver: &mpsc::Receiver<Value>, turn_id: &str, deadline: Instant) -> anyhow::Result<String> {
    let mut response = None;
    loop {
        let wait = deadline.saturating_duration_since(Instant::now());
        let event = receiver
            .recv_timeout(wait)
            .map_err(|_| anyhow::anyhow!("the title turn timed out"))?;
        let params = &event["params"];
        match event["method"].as_str() {
            Some("item/completed") if params["turnId"].as_str() == Some(turn_id) => {
                if params.pointer("/item/type").and_then(Value::as_str) == Some("agentMessage") {
                    let text = params.pointer("/item/text").and_then(Value::as_str).unwrap_or_default();
                    anyhow::ensure!(text.len() <= RESPONSE_MAX_BYTES, "the title response is too long");
                    response = Some(text.to_string());
                }
            }
            Some("turn/completed") if params.pointer("/turn/id").and_then(Value::as_str) == Some(turn_id) => {
                let status = params.pointer("/turn/status").and_then(Value::as_str).unwrap_or_default();
                anyhow::ensure!(status == "completed", "the title turn ended {status}");
                return response.ok_or_else(|| anyhow::anyhow!("the title turn completed without a response"));
            }
            _ => {}
        }
    }
}

fn output_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "title": { "type": "string", "minLength": 1, "maxLength": TITLE_MAX_CHARS } },
        "required": ["title"],
        "additionalProperties": false,
    })
}

/// The TUI's instructions (`tui/src/app/thread_title.rs`) + the message,
/// bounded to [`PROMPT_MAX_BYTES`] without splitting a character.
pub(crate) fn title_prompt(user_message: &str) -> String {
    let prefix = format!(
        "Generate a concise, single-line task title of at most {TITLE_MAX_CHARS} characters and under five words where possible. Start with an imperative verb. Capitalize only the first word unless the user's language, proper nouns, acronyms, or code terms require otherwise. Preserve ticket references exactly. Write in the user's language. Do not use quotes, markdown, or trailing punctuation. Do not answer the request.\n\nUser prompt:\n"
    );
    let remaining = PROMPT_MAX_BYTES.saturating_sub(prefix.len());
    let message: String = user_message
        .trim()
        .char_indices()
        .take_while(|(index, character)| index + character.len_utf8() <= remaining)
        .map(|(_, character)| character)
        .collect();
    format!("{prefix}{message}")
}

/// The `{title}` object (also inside a code fence), else — for providers
/// that ignore `outputSchema` (DeepSeek) — the first non-empty line; then the
/// TUI's normalization: one line, no quotes or trailing punctuation, at most
/// [`TITLE_MAX_CHARS`] characters.
pub(crate) fn parse_title(response: &str) -> Option<String> {
    let json = response
        .find('{')
        .zip(response.rfind('}'))
        .filter(|(start, end)| start < end)
        .and_then(|(start, end)| serde_json::from_str::<Value>(&response[start..=end]).ok());
    let title = match json {
        Some(value) => value.get("title")?.as_str()?.to_string(),
        None => {
            let line = response
                .lines()
                .map(|line| line.trim().trim_matches(['#', '*', '`', '-', ' ']).trim())
                .find(|line| !line.is_empty())?;
            let line = ["Title:", "title:", "标题：", "标题:"]
                .iter()
                .find_map(|prefix| line.strip_prefix(prefix))
                .unwrap_or(line);
            line.trim_matches(['#', '*', '`', ' ']).to_string()
        }
    };
    let normalized = title
        .trim()
        .trim_matches(|c| matches!(c, '"' | '\'' | '`' | '“' | '”' | '‘' | '’'))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches(['.', '?', '!'])
        .trim_end()
        .to_string();
    (!normalized.is_empty()).then(|| normalized.chars().take(TITLE_MAX_CHARS).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_is_bounded_without_splitting_characters() {
        let prompt = title_prompt(&"修复侧边栏".repeat(200));
        assert!(prompt.len() <= PROMPT_MAX_BYTES);
        assert!(prompt.starts_with("Generate a concise"));
        assert!(prompt.ends_with('栏') || prompt.ends_with('修') || prompt.ends_with('复') || prompt.ends_with('侧') || prompt.ends_with('边'));
        assert!(title_prompt("  fix it  ").ends_with("User prompt:\nfix it"));
    }

    #[test]
    fn titles_are_normalized_like_the_tui() {
        assert_eq!(parse_title(r#"{"title":"  “Fix  sidebar order.”  "}"#).as_deref(), Some("Fix sidebar order"));
        assert_eq!(parse_title(r#"{"title":"修复侧边栏排序？"}"#).as_deref(), Some("修复侧边栏排序？"));
        assert_eq!(parse_title(&format!(r#"{{"title":"{}"}}"#, "a".repeat(50))).unwrap().chars().count(), TITLE_MAX_CHARS);
        assert_eq!(parse_title("```json\n{\"title\": \"Pin chats on top\"}\n```").as_deref(), Some("Pin chats on top"));
        assert_eq!(parse_title("\n**Title:** \"Keep pinned chats on top.\"\n").as_deref(), Some("Keep pinned chats on top"));
        assert_eq!(parse_title("修复置顶聊天排序"), Some("修复置顶聊天排序".to_string()));
        assert_eq!(parse_title(r#"{"title":"   "}"#), None);
        assert_eq!(parse_title(r#"{"name":"x"}"#), None);
        assert_eq!(parse_title("  \n "), None);
    }

    #[test]
    fn response_collection_follows_the_turn() {
        let (sender, receiver) = mpsc::channel();
        sender.send(json!({"method": "item/completed", "params": {"turnId": "other", "item": {"type": "agentMessage", "text": "x"}}})).unwrap();
        sender.send(json!({"method": "item/completed", "params": {"turnId": "t1", "item": {"type": "agentMessage", "text": "{\"title\":\"Fix it\"}"}}})).unwrap();
        sender.send(json!({"method": "turn/completed", "params": {"turn": {"id": "t1", "status": "completed"}}})).unwrap();
        let text = collect_response(&receiver, "t1", Instant::now() + Duration::from_secs(1)).unwrap();
        assert_eq!(text, "{\"title\":\"Fix it\"}");

        let (sender, receiver) = mpsc::channel();
        sender.send(json!({"method": "turn/completed", "params": {"turn": {"id": "t1", "status": "failed"}}})).unwrap();
        assert!(collect_response(&receiver, "t1", Instant::now() + Duration::from_secs(1)).is_err());
        let (_sender, receiver) = mpsc::channel::<Value>();
        assert!(collect_response(&receiver, "t1", Instant::now() + Duration::from_millis(20)).is_err());
    }

    #[test]
    fn hidden_thread_events_are_claimed() {
        let (sender, receiver) = mpsc::channel();
        TITLE_THREADS.lock().unwrap().get_or_insert_with(HashMap::new).insert("title-thread".into(), sender);
        let event = |thread: &str| {
            AppServerEvent::Notification(cutex::app_server::protocol::RpcNotification {
                method: "turn/completed".into(),
                params: Some(json!({"turn": {"id": "t", "threadId": thread}, "threadId": thread})),
                raw: Value::Null,
            })
        };
        assert!(claim_event(&event("title-thread")));
        assert!(!claim_event(&event("main-thread")));
        assert_eq!(receiver.try_recv().unwrap()["method"], "turn/completed");
        TITLE_THREADS.lock().unwrap().as_mut().unwrap().remove("title-thread");
    }
}
