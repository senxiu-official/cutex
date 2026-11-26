use std::net::TcpStream;
use std::sync::OnceLock;

use anyhow::{ensure, Context};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ring::hmac;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::management::server::ManagementRequestContext;
use crate::management::users::Scope;
use crate::session::metadata::{
    cutex_session_launch_cwd, cutex_session_presented_name, session_cwd_identity,
};
use crate::session::model::CutexSessionRecord;

use super::history::{search_turn_page, HistorySearchSource};
use super::server::write_v2_error;

pub(super) const PATH: &str = "/v2/sessions/search";
pub(super) const FEATURE: &str = "session-search-v1";
const SESSION_LIMIT: usize = 30;
const TURN_LIMIT: u32 = 12;
const TASK_SCAN_LIMIT: usize = 4;
const MAX_TEXT_CHARS: usize = 16_384;

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
enum Mode {
    #[default]
    Sessions,
    Tasks,
}

#[derive(Default, Serialize)]
struct Query {
    mode: Mode,
    cwd: Option<String>,
    needle: String,
    retired: bool,
    session: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Position {
    after: String,
    resume: Option<Resume>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Resume {
    thread: String,
    generation: u64,
    source: HistorySearchSource,
    cursor: String,
}

fn parse(path: &str) -> anyhow::Result<(Query, Option<String>)> {
    ensure!(path.len() <= 16 * 1024, "search query is too long");
    let mut query = Query::default();
    let mut token = None;
    let mut seen = std::collections::HashSet::new();
    if let Some((_, raw)) = path.split_once('?') {
        for (key, value) in url::form_urlencoded::parse(raw.as_bytes()) {
            ensure!(
                seen.insert(key.clone()),
                "search parameter must not be repeated"
            );
            match key.as_ref() {
                "mode" => {
                    query.mode = match value.as_ref() {
                        "sessions" => Mode::Sessions,
                        "tasks" => Mode::Tasks,
                        _ => anyhow::bail!("mode must be sessions or tasks"),
                    }
                }
                "cwd" => {
                    query.cwd = Some(
                        directory(value.trim(), cfg!(windows))
                            .context("cwd must be an absolute directory path")?,
                    )
                }
                "q" => {
                    ensure!(value.chars().count() <= 128, "search text is too long");
                    query.needle = fold(&words(&value));
                }
                "lifecycle" => {
                    query.retired = match value.as_ref() {
                        "active" => false,
                        "retired" => true,
                        _ => anyhow::bail!("lifecycle must be active or retired"),
                    }
                }
                "sessionId" => {
                    ensure!(
                        !value.is_empty()
                            && value.len() <= 512
                            && !value.chars().any(char::is_control),
                        "invalid sessionId"
                    );
                    query.session = Some(value.into_owned());
                }
                "cursor" => token = Some(value.into_owned()),
                _ => anyhow::bail!("unknown search parameter"),
            }
        }
    }
    ensure!(
        query.mode != Mode::Tasks || !query.needle.is_empty(),
        "task search requires q"
    );
    Ok((query, token))
}

fn directory(raw: &str, windows: bool) -> Option<String> {
    if raw.chars().any(char::is_control) || raw.len() > 2048 {
        return None;
    }
    if windows {
        let canonical = session_cwd_identity(&raw.replace('/', "\\"));
        let bytes = canonical.as_bytes();
        let drive = bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && &bytes[1..3] == b":\\";
        let unc = canonical.strip_prefix("\\\\").is_some_and(|rest| {
            !rest.starts_with(['.', '?']) && rest.split('\\').filter(|s| !s.is_empty()).count() >= 2
        });
        if !drive && !unc {
            return None;
        }
        let canonical = canonical.trim_end_matches('\\').replace('\\', "/");
        return (!canonical.split('/').any(|s| s == "." || s == ".."))
            .then(|| canonical.to_lowercase());
    }
    if !raw.starts_with('/') || raw.split('/').any(|s| s == "." || s == "..") {
        return None;
    }
    Some(format!(
        "/{}",
        raw.split('/')
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("/")
    ))
}

fn cwd_matches(cwd: &str, directory_filter: &str, windows: bool) -> bool {
    let Some(cwd) = directory(cwd, windows) else {
        return false;
    };
    cwd == directory_filter
        || cwd
            .strip_prefix(directory_filter)
            .is_some_and(|tail| directory_filter.ends_with('/') || tail.starts_with('/'))
}

fn words(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn fold(text: &str) -> String {
    text.chars().flat_map(char::to_lowercase).collect()
}

// Native page cursors are opaque and go back only to the same server-selected
// thread and reader. Authenticate the position so a client cannot substitute
// another reader's cursor. Every page also re-evaluates the caller's scope.
fn cursor_key() -> &'static hmac::Key {
    static KEY: OnceLock<hmac::Key> = OnceLock::new();
    KEY.get_or_init(|| hmac::Key::new(hmac::HMAC_SHA256, uuid::Uuid::new_v4().as_bytes()))
}

fn query_digest(query: &Query) -> anyhow::Result<String> {
    let bytes = serde_json::to_vec(query)?;
    Ok(URL_SAFE_NO_PAD.encode(ring::digest::digest(&ring::digest::SHA256, &bytes).as_ref()))
}

fn encode(query: &Query, position: Position) -> anyhow::Result<String> {
    let payload = serde_json::to_vec(&(query_digest(query)?, position))?;
    let mut token = hmac::sign(cursor_key(), &payload).as_ref().to_vec();
    token.extend(payload);
    Ok(URL_SAFE_NO_PAD.encode(token))
}

fn decode(query: &Query, token: &str) -> anyhow::Result<Position> {
    let raw = URL_SAFE_NO_PAD.decode(token)?;
    ensure!(raw.len() > 32, "invalid search cursor");
    let (signature, payload) = raw.split_at(32);
    hmac::verify(cursor_key(), payload, signature)
        .map_err(|_| anyhow::anyhow!("invalid search cursor"))?;
    let (saved, position): (String, Position) = serde_json::from_slice(payload)?;
    ensure!(
        saved == query_digest(query)?,
        "search cursor belongs to another query"
    );
    Ok(position)
}

fn eligible(
    row: &CutexSessionRecord,
    query: &Query,
    host: &str,
    registry: &crate::im::registry::ImRegistry,
    scope: Option<&Scope>,
) -> bool {
    crate::runtime::lifecycle::cutex_session_host_is_local(&row.host_id, host)
        && super::session::is_durable_management_session(row)
        && row.is_retired() == query.retired
        && (row.is_retired() || super::session::session_is_visible(registry, row))
        && scope.is_none_or(|scope| scope.sees_session(&row.cutex_session_id))
        && query
            .session
            .as_deref()
            .is_none_or(|id| id == row.cutex_session_id)
        && query
            .cwd
            .as_deref()
            .is_none_or(|cwd| cwd_matches(cutex_session_launch_cwd(row), cwd, cfg!(windows)))
}

fn anchor(row: &CutexSessionRecord, host: &str) -> Value {
    json!({"hostId":host,"cutexSessionId":row.cutex_session_id,"displayName":cutex_session_presented_name(row),
        "cwd":cutex_session_launch_cwd(row),"lifecycle":row.archive_state.label(),"updatedAt":row.updated_at})
}

pub(super) fn handle(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    context: ManagementRequestContext,
    scope: Option<&Scope>,
) -> anyhow::Result<()> {
    let (query, position) = match parse(&request.path).and_then(|(query, token)| {
        let position = token
            .as_deref()
            .map(|token| {
                decode(&query, token)
                    .map_err(|_| anyhow::anyhow!("search cursor is invalid or expired"))
            })
            .transpose()?;
        Ok((query, position))
    }) {
        Ok(parsed) => parsed,
        Err(error) => {
            return write_v2_error(
                stream,
                400,
                "Bad Request",
                "invalid_request",
                &error.to_string(),
                false,
                json!({}),
            )
        }
    };
    if scope.is_some() && query.retired {
        return write_v2_error(
            stream,
            403,
            "Forbidden",
            "owner_required",
            "archived session search requires the owner",
            false,
            json!({}),
        );
    }

    let host = crate::platform::host::current_host_name();
    let registry = (context.load_registry)()?;
    let (rows, more) = crate::session::store::with_cutex_session_store(|store| {
        let mut candidates: Vec<_> = store
            .sessions
            .values()
            .filter(|row| {
                let after = position.as_ref().is_none_or(|pos| {
                    row.cutex_session_id > pos.after
                        || (row.cutex_session_id == pos.after && pos.resume.is_some())
                });
                after && eligible(row, &query, &host, &registry, scope)
            })
            .filter(|row| match query.mode {
                Mode::Sessions => {
                    query.needle.is_empty()
                        || fold(&cutex_session_presented_name(row)).contains(&query.needle)
                        || row
                            .formal_agent_name
                            .as_ref()
                            .is_some_and(|name| fold(name).contains(&query.needle))
                }
                Mode::Tasks => row.app_server_thread_id().is_some_and(|id| !id.is_empty()),
            })
            .collect();
        // The durable session map is hash-ordered; the cursor uses session IDs.
        candidates
            .sort_unstable_by(|left, right| left.cutex_session_id.cmp(&right.cutex_session_id));
        let count = if query.mode == Mode::Sessions {
            SESSION_LIMIT
        } else {
            TASK_SCAN_LIMIT
        };
        let more = candidates.len() > count;
        let rows = candidates
            .into_iter()
            .take(count)
            .cloned()
            .collect::<Vec<_>>();
        (rows, more)
    })?;
    let (items, next, partial) = if query.mode == Mode::Sessions {
        let next = rows.last().filter(|_| more).map(|row| Position {
            after: row.cutex_session_id.clone(),
            resume: None,
        });
        (
            rows.iter().map(|row| anchor(row, &host)).collect(),
            next,
            false,
        )
    } else {
        search_tasks(&rows, &query, position.as_ref(), more, &host, context)?
    };
    let more = next.is_some();
    let next = next.map(|position| encode(&query, position)).transpose()?;
    write_json_response(
        stream,
        200,
        "OK",
        &json!({
            "contractVersion": 2, "hostId": host, "mode": query.mode,
            "items": items, "nextCursor": next, "more": more, "partial": partial,
        }),
    )
}

fn search_tasks(
    rows: &[CutexSessionRecord],
    query: &Query,
    position: Option<&Position>,
    more_rows: bool,
    host: &str,
    context: ManagementRequestContext,
) -> anyhow::Result<(Vec<Value>, Option<Position>, bool)> {
    let mut remaining = TASK_SCAN_LIMIT;
    let mut partial = false;
    for (index, row) in rows.iter().enumerate() {
        let mut resume = position
            .filter(|pos| pos.after == row.cutex_session_id)
            .and_then(|pos| pos.resume.clone());
        loop {
            let more = more_rows || index + 1 < rows.len();
            let (items, next, incomplete) =
                task_page(row, query, resume.as_ref(), more, host, context)?;
            partial |= incomplete;
            remaining -= 1;
            if !items.is_empty() || remaining == 0 || next.is_none() {
                return Ok((items, next, partial));
            }
            resume = next.and_then(|position| position.resume);
            if resume.is_none() {
                break;
            }
        }
    }
    Ok((Vec::new(), None, partial))
}

fn task_page(
    row: &CutexSessionRecord,
    query: &Query,
    resume: Option<&Resume>,
    more: bool,
    host: &str,
    context: ManagementRequestContext,
) -> anyhow::Result<(Vec<Value>, Option<Position>, bool)> {
    let thread = row
        .app_server_thread_id()
        .context("session has no thread")?;
    let session = &row.cutex_session_id;
    let generation = row.runtime_generation;
    let tail = || {
        more.then(|| Position {
            after: session.clone(),
            resume: None,
        })
    };
    if resume.is_some_and(|saved| saved.thread != thread || saved.generation != generation) {
        return Ok((Vec::new(), tail(), true));
    }
    let connected = !row.is_retired()
        && (context.load_runtime_status)(session)?.is_some_and(|status| {
            status.connected
                && status.cutex_session_id == *session
                && status.thread_id == thread
                && status.runtime_generation == generation
        });
    let reader = resume.map(|saved| (saved.source, saved.cursor.as_str()));
    let page = search_turn_page(
        session,
        thread,
        connected,
        TURN_LIMIT,
        reader,
        context.forward_native_request,
        context.open_offline_history,
    );
    let Ok((turns, cursor, source)) = page else {
        // Do not expose runtime error bodies. Other authorized sessions can
        // still be searched, including when an old runtime has no paging.
        return Ok((Vec::new(), tail(), true));
    };
    let mut partial = false;
    let mut items = Vec::new();
    for turn in turns {
        if let Some(hit) = task_hit(&turn, &query.needle, &mut partial) {
            let mut item = anchor(row, host);
            item.as_object_mut().unwrap().extend(hit);
            items.push(item);
        }
    }
    let next = cursor
        .filter(|cursor| {
            let valid = cursor.len() <= 1536 && reader.is_none_or(|(_, old)| old != cursor);
            partial |= !valid;
            valid
        })
        .map(|cursor| Position {
            after: session.clone(),
            resume: Some(Resume {
                thread: thread.into(),
                generation,
                source,
                cursor,
            }),
        });
    Ok((items, next.or_else(tail), partial))
}

fn task_hit(
    turn: &Value,
    needle: &str,
    partial: &mut bool,
) -> Option<serde_json::Map<String, Value>> {
    if !matches!(
        turn["status"].as_str(),
        Some("completed" | "failed" | "interrupted")
    ) {
        return None;
    }
    *partial |= turn["truncated"].as_bool() == Some(true)
        || turn["itemsView"].as_str() == Some("notLoaded");
    let id = turn["id"].as_str().filter(|id| {
        !id.is_empty() && id.len() <= 512 && !id.contains('/') && !id.chars().any(char::is_control)
    });
    let (Some(id), Some(items)) = (id, turn["items"].as_array()) else {
        *partial = true;
        return None;
    };
    let prompt = items.iter().find(|item| item["type"] == "userMessage");
    let reply = items.iter().rfind(|item| item["type"] == "agentMessage");
    let prompt_text = prompt
        .and_then(|item| item["content"].as_array())
        .map(|content| {
            content
                .iter()
                .filter(|part| part["type"] == "text")
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    let reply_text = reply
        .and_then(|item| item["text"].as_str())
        .unwrap_or_default();
    let snippet =
        snippet(&prompt_text, needle, partial).or_else(|| snippet(reply_text, needle, partial))?;
    let title = words(&prompt_text.chars().take(MAX_TEXT_CHARS).collect::<String>())
        .chars()
        .take(120)
        .collect::<String>();
    let item = json!({"turnId":id,"title":title,"snippet":snippet,"turnStatus":turn["status"]});
    item.as_object().cloned()
}

fn snippet(text: &str, needle: &str, partial: &mut bool) -> Option<String> {
    let mut chars = text.chars();
    let text = words(&chars.by_ref().take(MAX_TEXT_CHARS).collect::<String>());
    *partial |= chars.next().is_some();
    let mut folded = String::new();
    let mut offsets = Vec::new();
    for (index, ch) in text.char_indices() {
        for lower in ch.to_lowercase() {
            folded.push(lower);
            offsets.resize(folded.len(), index);
        }
    }
    let found = offsets[folded.find(needle)?];
    let start = text[..found]
        .char_indices()
        .rev()
        .nth(55)
        .map_or(0, |(byte, _)| byte);
    let mut excerpt = text[start..].chars();
    let body: String = excerpt.by_ref().take(220).collect();
    Some(format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        body,
        if excerpt.next().is_some() { "…" } else { "" }
    ))
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cwd_matches_directory_components_with_host_spelling() {
        assert!(cwd_matches("/work//Repo/sub", "/work/Repo", false));
        assert!(!cwd_matches("/work/Repo-other", "/work/Repo", false));
        assert!(!cwd_matches("/work/repo", "/work/Repo", false));
        assert!(cwd_matches("/anything", "/", false));
        let root = directory(r"C:\", true).unwrap();
        let repo = directory(r"\\?\C:\Repo", true).unwrap();
        assert!(cwd_matches(r"C:\repo\child", &root, true));
        assert!(cwd_matches(r"c:/repo/child", &repo, true));
        assert!(!cwd_matches(r"C:\repository\child", &repo, true));
        let share = directory(r"\\?\UNC\Host\Share", true).unwrap();
        assert!(cwd_matches(r"\\host\share\directory", &share, true));
        assert!(directory(r"C:repo", true).is_none());
        assert!(directory("/work/../repo", false).is_none());
    }

    #[test]
    fn cursor_is_bound_to_filters_and_authenticated() {
        let make = |q| parse(q).unwrap().0;
        let query = make("/v2/sessions/search?mode=tasks&q=Owner+Task");
        let cursor = encode(
            &query,
            Position {
                after: "cutex.current".into(),
                resume: Some(Resume {
                    thread: "thread-id".into(),
                    generation: 42,
                    source: HistorySearchSource::Offline,
                    cursor: "native".into(),
                }),
            },
        )
        .unwrap();
        let position = decode(&query, &cursor).unwrap();
        assert_eq!(position.after, "cutex.current");
        assert_eq!(position.resume.unwrap().cursor, "native");
        assert!(decode(&make("/v2/sessions/search?mode=tasks&q=other"), &cursor).is_err());
        let mut raw = URL_SAFE_NO_PAD.decode(&cursor).unwrap();
        raw[0] ^= 0x80;
        assert!(decode(&query, &URL_SAFE_NO_PAD.encode(raw)).is_err());
        assert!(parse("/v2/sessions/search?mode=tasks&q=").is_err());
        assert!(parse("/v2/sessions/search?q=one&q=two").is_err());
        assert!(parse("/v2/sessions/search?unexpected=1").is_err());
    }

    #[test]
    fn task_hits_anchor_prompt_or_final_reply_and_skip_tool_outputs() {
        let turn = json!({"id":"matching-turn","status":"completed","itemsView":"summary","items":[
            {"type":"userMessage","content":[{"type":"text","text":"Make the book"}]},
            {"type":"commandExecution","aggregatedOutput":"secret needle"},
            {"type":"agentMessage","text":"first commentary only"},
            {"type":"agentMessage","text":"Completed: İNDEX 文件"},
        ]});
        let mut partial = false;
        let prompt_hit = task_hit(&turn, "the book", &mut partial).unwrap();
        assert_eq!(prompt_hit["turnId"], "matching-turn");
        assert_eq!(prompt_hit["title"], "Make the book");
        assert_eq!(
            task_hit(&turn, "i\u{307}ndex 文件", &mut partial).unwrap()["snippet"],
            "Completed: İNDEX 文件"
        );
        assert!(task_hit(&turn, "secret needle", &mut partial).is_none());
        assert!(task_hit(&turn, "first commentary", &mut partial).is_none());
        assert!(!partial);
        let mut active = turn.clone();
        active["status"] = json!("inProgress");
        assert!(task_hit(&active, "the book", &mut partial).is_none());
        let mut truncated = turn;
        truncated["truncated"] = json!(true);
        assert!(task_hit(&truncated, "missing", &mut partial).is_none());
        assert!(partial);
    }
}
