//! Successful Cutex-managed replies to timeline questions and approvals.
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OpenFlags, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::http::server::SimpleHttpRequest;
use crate::management::users::{current_principal, Principal};

pub const FEATURE: &str = "session-interaction-history-v1";
const DEFAULT_LIMIT: u32 = 20;
const MAX_LIMIT: u32 = 50;
const MAX_TURNS: usize = 50;
const RETAINED_PER_SESSION: i64 = 256;
const MAX_ROW_BYTES: usize = 512 * 1024;
const MAX_APPROVAL_TEXT_BYTES: usize = 16 * 1024;
const PAGE_BYTES: i64 = 2 * 1024 * 1024;
const CURSOR_PREFIX: &str = "cutex-interaction:";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Actor {
    Owner,
    User { user_id: String, name: String },
    Agent { cutex_session_id: String },
}

impl Actor {
    pub(crate) fn current() -> Self {
        match current_principal() {
            Some(Principal::User { id, name, .. }) => Self::User { user_id: id, name },
            _ => Self::Owner,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Interaction {
    runtime_generation: u64,
    native: Value,
    turn_id: Option<String>,
    result: Value,
    actor: Actor,
    answered_at: DateTime<Utc>,
    redacted_question_ids: Vec<String>,
    #[serde(default)]
    truncated: bool,
}

fn select_fields(value: &Value, fields: &[&str]) -> Map<String, Value> {
    fields
        .iter()
        .filter_map(|key| value.get(key).map(|value| (key.to_string(), value.clone())))
        .collect()
}

fn project_options(params: &mut Map<String, Value>) {
    if let Some(options) = params.get_mut("options").and_then(Value::as_array_mut) {
        for option in options {
            *option = Value::Object(select_fields(option, &["label", "description"]));
        }
    }
}

fn project_question(question: &Value) -> Value {
    let mut params = select_fields(
        question,
        &["id", "header", "question", "isOther", "isSecret", "options"],
    );
    project_options(&mut params);
    if let Some(options) = question.get("cutexQuestionOptions") {
        let mut options =
            select_fields(options, &["version", "question", "multiSelect", "options"]);
        project_options(&mut options);
        params.insert("cutexQuestionOptions".into(), Value::Object(options));
    }
    Value::Object(params)
}

fn truncate_text(value: &mut Value, truncated: &mut bool) {
    if let Value::String(text) = value {
        if text.len() > MAX_APPROVAL_TEXT_BYTES {
            let mut end = MAX_APPROVAL_TEXT_BYTES;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            *truncated = true;
        }
    }
}

fn truncate_strings(values: Option<&mut Value>, truncated: &mut bool) {
    if let Some(values) = values.and_then(Value::as_array_mut) {
        for value in values {
            truncate_text(value, truncated);
        }
    }
}

fn truncate_decision(decision: &mut Value, truncated: &mut bool) {
    for path in [
        "/acceptWithExecpolicyAmendment/execpolicy_amendment",
        "/acceptWithClaudeRules/rules",
    ] {
        truncate_strings(decision.pointer_mut(path), truncated);
    }
}

impl Interaction {
    fn new(
        runtime_generation: u64,
        native: Value,
        reply: &Value,
        actor: Actor,
        answered_at: DateTime<Utc>,
    ) -> Option<Self> {
        let reply_result = reply.get("result")?;
        let id = native.get("id")?;
        let method = native.get("method")?.as_str()?;
        let native_params = &native["params"];
        let mut params = select_fields(native_params, &["turnId", "itemId"]);
        let mut redacted_question_ids = Vec::new();
        let mut truncated = false;
        let result = match method {
            "item/tool/requestUserInput" => {
                let questions = native_params.get("questions")?.as_array()?;
                let mut answers = reply_result.get("answers")?.as_object()?.clone();
                for question in questions {
                    if question["isSecret"] == true {
                        let id = question["id"].as_str().filter(|id| !id.is_empty())?;
                        let had_answer = answers.remove(id).is_some_and(|answer| {
                            answer["answers"]
                                .as_array()
                                .is_some_and(|values| !values.is_empty())
                        });
                        if had_answer {
                            redacted_question_ids.push(id.to_owned());
                        }
                    }
                }
                params.extend(select_fields(native_params, &["isBlocking"]));
                params.insert(
                    "questions".into(),
                    Value::Array(questions.iter().map(project_question).collect()),
                );
                json!({"answers": answers})
            }
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                let fields: &[&str] = if method == "item/commandExecution/requestApproval" {
                    &["kind", "command", "cwd", "reason", "availableDecisions"]
                } else {
                    &["reason", "grantRoot", "claudePermissionRules"]
                };
                params.extend(select_fields(native_params, fields));
                truncate_strings(params.get_mut("claudePermissionRules"), &mut truncated);
                if let Some(decisions) = params
                    .get_mut("availableDecisions")
                    .and_then(Value::as_array_mut)
                {
                    for decision in decisions {
                        truncate_decision(decision, &mut truncated);
                    }
                }
                let mut decision = reply_result.get("decision")?.clone();
                truncate_decision(&mut decision, &mut truncated);
                json!({"decision": decision})
            }
            "item/permissions/requestApproval" => {
                params.extend(select_fields(
                    native_params,
                    &["reason", "cwd", "permissions"],
                ));
                json!({"permissions": reply_result.get("permissions")?, "scope": reply_result.get("scope")?})
            }
            _ => return None,
        };
        for key in ["command", "reason", "cwd", "grantRoot"] {
            if let Some(value) = params.get_mut(key) {
                truncate_text(value, &mut truncated);
            }
        }
        let turn_id = params
            .get("turnId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
        Some(Self {
            runtime_generation,
            native: json!({"id": id, "method": method, "params": params}),
            turn_id,
            result,
            actor,
            answered_at,
            redacted_question_ids,
            truncated,
        })
    }
}

pub(super) fn session_id_from_path(path: &str) -> Option<String> {
    let session = path
        .strip_prefix("/v2/sessions/")?
        .strip_suffix("/history/interactions")?;
    super::server::decode_path_segment(session)
}

pub(super) struct Query {
    limit: u32,
    before: Option<i64>,
    turn_ids: Option<Vec<String>>,
}

impl Query {
    pub(super) fn parse(request: &SimpleHttpRequest) -> Result<Self, String> {
        let mut params = super::workspace::query_params(request);
        let limit = params.remove("limit").map_or(Ok(DEFAULT_LIMIT), |value| {
            value
                .parse::<u32>()
                .ok()
                .filter(|limit| (1..=MAX_LIMIT).contains(limit))
                .ok_or_else(|| format!("limit must be between 1 and {MAX_LIMIT}"))
        })?;
        let before = params
            .remove("cursor")
            .map(|cursor| {
                cursor
                    .strip_prefix(CURSOR_PREFIX)
                    .and_then(|seq| seq.parse::<i64>().ok())
                    .filter(|seq| *seq > 0)
                    .ok_or_else(|| "invalid interaction history cursor".to_owned())
            })
            .transpose()?;
        let turn_ids = params
            .remove("turnIds")
            .map(|value| {
                serde_json::from_str::<Vec<String>>(&value)
                    .ok()
                    .filter(|ids| {
                        (1..=MAX_TURNS).contains(&ids.len())
                            && ids.iter().all(|id| {
                                !id.is_empty()
                                    && id.len() <= 512
                                    && !id.chars().any(char::is_control)
                            })
                    })
                    .ok_or_else(|| {
                        format!(
                            "turnIds must be a JSON array of 1 to {MAX_TURNS} nonempty turn IDs"
                        )
                    })
            })
            .transpose()?;
        if !params.is_empty() {
            return Err("unknown interaction history query parameter".into());
        }
        Ok(Self {
            limit,
            before,
            turn_ids,
        })
    }
}

fn path() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::runtime_dir()?
        .join("management-v2/interaction-history/history.sqlite3"))
}

fn open_writer(path: &Path) -> anyhow::Result<Connection> {
    crate::management::client_tokens::ensure_private_dir(
        path.parent().expect("history directory"),
    )?;
    let db = Connection::open(path)?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.pragma_update(None, "synchronous", "FULL")?;
    db.execute_batch(
        "BEGIN IMMEDIATE;
         CREATE TABLE IF NOT EXISTS interactions (
             seq INTEGER NOT NULL,
             session_id TEXT NOT NULL,
             runtime_generation INTEGER NOT NULL,
             request_id TEXT NOT NULL,
             turn_id TEXT,
             data BLOB NOT NULL,
             PRIMARY KEY(session_id, seq),
             UNIQUE(session_id, runtime_generation, request_id)
         );
         CREATE INDEX IF NOT EXISTS interactions_turn ON interactions(session_id, turn_id, seq DESC);
         COMMIT;",
    )?;
    Ok(db)
}

fn with_writer<T>(
    path: &Path,
    write: impl FnOnce(&mut Connection) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    // Tests rotate private homes and need to release their Windows file handles.
    #[cfg(test)]
    return write(&mut open_writer(path)?);

    #[cfg(not(test))]
    {
        use std::sync::{Mutex, OnceLock};
        type Writer = Option<(PathBuf, Connection)>;
        static WRITER: OnceLock<Mutex<Writer>> = OnceLock::new();
        let mut slot = WRITER
            .get_or_init(Default::default)
            .lock()
            .map_err(|_| anyhow::anyhow!("interaction history writer lock poisoned"))?;
        if let Some((cached_path, db)) = slot.as_mut() {
            if cached_path.as_path() == path {
                return write(db);
            }
        }
        let mut db = open_writer(path)?;
        let result = write(&mut db);
        *slot = Some((path.to_owned(), db));
        result
    }
}

fn insert(db: &mut Connection, session: &str, entry: Interaction) -> anyhow::Result<()> {
    let data = serde_json::to_vec(&entry)?;
    if data.len() > MAX_ROW_BYTES {
        return Ok(());
    }
    let generation = i64::try_from(entry.runtime_generation)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let inserted = tx.execute(
        "INSERT INTO interactions(session_id, seq, runtime_generation, request_id, turn_id, data)
         VALUES (?1, (SELECT coalesce(max(seq), 0) + 1 FROM interactions WHERE session_id = ?1), ?2, ?3, ?4, ?5)
         ON CONFLICT(session_id, runtime_generation, request_id) DO NOTHING",
        params![session, generation, entry.native["id"].to_string(), entry.turn_id, data],
    )?;
    if inserted != 0 {
        tx.execute(
            "DELETE FROM interactions WHERE session_id = ?1 AND seq < (
                SELECT seq FROM interactions WHERE session_id = ?1
                ORDER BY seq DESC LIMIT 1 OFFSET ?2
             )",
            params![session, RETAINED_PER_SESSION - 1],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub(super) fn record_submitted(
    session: &str,
    generation: u64,
    native: Value,
    reply: &Value,
    actor: Actor,
    at: DateTime<Utc>,
) -> anyhow::Result<()> {
    if let Some(entry) = Interaction::new(generation, native, reply, actor, at) {
        with_writer(&path()?, |db| insert(db, session, entry))?;
    }
    Ok(())
}

fn page(
    path: &Path,
    session: &str,
    query: &Query,
) -> anyhow::Result<(Vec<Interaction>, Option<String>)> {
    if !path.try_exists()? {
        return Ok((vec![], None));
    }
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(Duration::from_secs(5))?;
    let exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name = 'interactions')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok((vec![], None)); // The first writer has yet to commit the schema.
    }
    let sql = if query.turn_ids.is_some() {
        "SELECT seq, length(data), data FROM interactions WHERE session_id = ?1 AND seq < ?2
         AND turn_id IN (SELECT value FROM json_each(?4)) ORDER BY seq DESC LIMIT ?3"
    } else {
        "SELECT seq, length(data), data FROM interactions WHERE session_id = ?1 AND seq < ?2
         ORDER BY seq DESC LIMIT ?3"
    };
    let mut stmt = db.prepare(sql)?;
    let before = query.before.unwrap_or(i64::MAX);
    let limit = query.limit + 1;
    let mut rows = match &query.turn_ids {
        Some(turns) => stmt.query(params![
            session,
            before,
            limit,
            serde_json::to_string(turns)?
        ])?,
        None => stmt.query(params![session, before, limit])?,
    };
    let mut records = Vec::new();
    let mut total_bytes = 0i64;
    let mut last_seq = 0i64;
    while let Some(row) = rows.next()? {
        let size: i64 = row.get(1)?;
        if records.len() == query.limit as usize
            || (!records.is_empty() && total_bytes.saturating_add(size) > PAGE_BYTES)
        {
            return Ok((records, Some(format!("{CURSOR_PREFIX}{last_seq}"))));
        }
        records.push(serde_json::from_slice(row.get_ref(2)?.as_blob()?)?);
        total_bytes = total_bytes.saturating_add(size);
        last_seq = row.get(0)?;
    }
    Ok((records, None))
}

pub(super) fn resource(session: &str, query: &Query) -> anyhow::Result<Value> {
    let (interactions, next_cursor) = page(&path()?, session, query)?;
    Ok(
        json!({"contractVersion": 2, "cutexSessionId": session, "interactions": interactions, "nextCursor": next_cursor}),
    )
}

pub fn forget_deleted_session(session: &str) -> anyhow::Result<()> {
    let path = path()?;
    if path.try_exists()? {
        with_writer(&path, |db| {
            db.execute("DELETE FROM interactions WHERE session_id = ?1", [session])?;
            Ok(())
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_committed_reply_wins_and_request_ids_can_repeat_in_later_generations() {
        let dir = std::env::temp_dir().join(format!("cutex-interactions-{}", uuid::Uuid::new_v4()));
        let path = dir.join("history.sqlite3");
        let query = Query {
            limit: 20,
            before: None,
            turn_ids: None,
        };
        let now = Utc::now();
        let request = json!({"id":9,"method":"item/fileChange/requestApproval","params":{"turnId":"t","grantRoot":"/fixture"}});
        let entry = |generation, decision, actor| {
            Interaction::new(
                generation,
                request.clone(),
                &json!({"result":{"decision":decision}}),
                actor,
                now,
            )
            .unwrap()
        };
        assert!(page(&path, "session", &query).unwrap().0.is_empty());
        let mut db = open_writer(&path).unwrap();
        insert(&mut db, "session", entry(1, "accept", Actor::Owner)).unwrap();
        insert(&mut db, "session", entry(1, "decline", Actor::Owner)).unwrap();
        insert(&mut db, "session", entry(2, "decline", Actor::Owner)).unwrap();
        insert(&mut db, "other", entry(1, "acceptForSession", Actor::Owner)).unwrap();
        let (records, cursor) = page(&path, "session", &query).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].runtime_generation, 2);
        assert_eq!(records[0].result["decision"], "decline");
        assert_eq!(records[1].runtime_generation, 1);
        assert_eq!(records[1].result["decision"], "accept");
        assert!(cursor.is_none());
        assert_eq!(page(&path, "other", &query).unwrap().0.len(), 1);
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn delegated_actor_and_time_survive_serialization_and_secret_values_are_absent() {
        let now = DateTime::parse_from_rfc3339("2026-11-25T14:16:32Z")
            .unwrap()
            .with_timezone(&Utc);
        let entry = Interaction::new(
            3,
            json!({"id":"q", "method":"item/tool/requestUserInput","params":{"turnId":"t","questions":[
                {"id":"a","isSecret":true}, {"id":"b","isSecret":false}
            ]}}),
            &json!({"result":{"answers":{"a":{"answers":["secret-value"]},"b":{"answers":["public-value"]}}}}),
            Actor::Agent { cutex_session_id: "requester".into() },
            now,
        ).unwrap();
        let value = serde_json::to_value(&entry).unwrap();
        assert_eq!(
            value["actor"],
            json!({"kind":"agent","cutexSessionId":"requester"})
        );
        assert_eq!(value["answeredAt"], "2026-11-25T14:16:32Z");
        assert_eq!(
            value["result"]["answers"],
            json!({"b":{"answers":["public-value"]}})
        );
        assert_eq!(value["redactedQuestionIds"], json!(["a"]));
        assert!(!value.to_string().contains("secret-value"));
    }

    #[test]
    fn a_secret_question_without_a_string_id_prevents_recording_any_answers() {
        let request = json!({"id":1,"method":"item/tool/requestUserInput","params":{
            "questions":[{"id":1,"isSecret":true}]}});
        let reply = json!({"result":{"answers":{"1":{"answers":["secret"]}}}});
        assert!(Interaction::new(1, request.clone(), &reply, Actor::Owner, Utc::now()).is_none());
        let empty_reply = json!({"result":{"answers":{}}});
        assert!(Interaction::new(1, request, &empty_reply, Actor::Owner, Utc::now()).is_none());
    }

    #[test]
    fn approval_projects_rendered_fields_and_caps_utf8_and_structured_command_text() {
        let command = "界".repeat(MAX_APPROVAL_TEXT_BYTES / 3 + 1);
        let decision = json!({"acceptWithExecpolicyAmendment":{"execpolicy_amendment":[command]}});
        let request = json!({"id":1,"method":"item/commandExecution/requestApproval",
            "params":{"turnId":"turn-1","itemId":"item-1","command":command,"reason":"Review",
                "availableDecisions":[decision],"commandActions":[{"command":command}], "threadId":"other"}});
        let entry = Interaction::new(
            1,
            request,
            &json!({"result":{"decision":decision}}),
            Actor::Owner,
            Utc::now(),
        )
        .unwrap();
        let params = &entry.native["params"];
        assert_eq!(params.as_object().unwrap().len(), 5);
        assert_eq!(params["turnId"], "turn-1");
        assert_eq!(params["itemId"], "item-1");
        assert_eq!(params["reason"], "Review");
        assert_eq!(
            params["command"].as_str().unwrap().len(),
            MAX_APPROVAL_TEXT_BYTES - 1
        );
        assert_eq!(params["availableDecisions"][0], entry.result["decision"]);
        assert_eq!(
            entry.result["decision"]["acceptWithExecpolicyAmendment"]["execpolicy_amendment"][0],
            params["command"]
        );
        assert!(entry.truncated);
    }

    #[test]
    fn retention_keeps_newest_rows_and_does_not_prune_a_different_session() {
        let dir = std::env::temp_dir().join(format!(
            "cutex-interaction-retention-{}",
            uuid::Uuid::new_v4()
        ));
        let path = dir.join("history.sqlite3");
        let mut db = open_writer(&path).unwrap();
        let entry = |id| {
            Interaction::new(
                1,
                json!({"id":id,"method":"item/fileChange/requestApproval","params":{}}),
                &json!({"result":{"decision":"decline"}}),
                Actor::Owner,
                Utc::now(),
            )
            .unwrap()
        };
        insert(&mut db, "other", entry(1)).unwrap();
        for id in 1..=RETAINED_PER_SESSION + 1 {
            insert(&mut db, "session", entry(id)).unwrap();
        }
        let bounds: (i64, i64, i64) = db.query_row(
            "SELECT count(*), min(seq), max(seq) FROM interactions WHERE session_id = 'session'",
            [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        ).unwrap();
        assert_eq!(bounds, (RETAINED_PER_SESSION, 2, RETAINED_PER_SESSION + 1));
        let mut oversized = entry(RETAINED_PER_SESSION + 2);
        oversized.native["params"]["reason"] = json!("x".repeat(MAX_ROW_BYTES));
        insert(&mut db, "session", oversized).unwrap();
        let query = Query {
            limit: 1,
            before: None,
            turn_ids: None,
        };
        let (newest, _) = page(&path, "session", &query).unwrap();
        assert_eq!(newest[0].native["id"], RETAINED_PER_SESSION + 1);
        assert_eq!(page(&path, "other", &query).unwrap().0.len(), 1);
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
