//! Owner-only fork preview: no native request, session mutation or credential read.
use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::{
    profiles::{
        model::{AccountsStore, CliKind, StoredAccount},
        settings_api,
    },
    runtime::claude_accounts::{self, ClaudeAccountsFile},
    session::model::CutexSessionRecord,
};
use anyhow::{ensure, Context};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::{json, Value};
use std::{net::TcpStream, path::Path, time::Duration};

pub fn session_id_from_path(path: &str) -> Option<String> {
    let encoded = path
        .split('?')
        .next()?
        .strip_prefix("/v2/sessions/")?
        .strip_suffix("/fork-preview")?;
    let id = super::attachments::percent_decode(encoded)?;
    (!id.is_empty() && !id.contains(['/', '\\']) && !id.chars().any(char::is_control)).then_some(id)
}
pub fn is_path(path: &str) -> bool {
    session_id_from_path(path).is_some()
}

#[derive(Debug, Default, PartialEq)]
struct Query {
    after_turn_id: Option<String>,
    profile: Option<String>,
}
fn parse_query(path: &str) -> Result<Query, String> {
    let mut result = Query::default();
    let mut seen = std::collections::HashSet::new();
    for (key, value) in url::form_urlencoded::parse(
        path.split_once('?')
            .map(|(_, q)| q)
            .unwrap_or("")
            .as_bytes(),
    ) {
        if !seen.insert(key.to_string()) {
            return Err(format!("duplicate query parameter {key}"));
        }
        if !matches!(key.as_ref(), "afterTurnId" | "profile") {
            return Err(format!("unknown query parameter {key}"));
        }
        let trimmed = value.trim();
        if value.len() > 200 || value.chars().any(char::is_control) {
            return Err(format!("invalid {key}"));
        }
        let parsed = (!trimmed.is_empty()).then(|| trimmed.to_owned());
        if key == "afterTurnId" {
            result.after_turn_id = parsed;
        } else {
            result.profile = parsed;
        }
    }
    Ok(result)
}

/// Bypass the ordinary store opener: it can import/repair old stores. Preview
/// never creates a database, lock, sentinel, event repository, or migration.
fn read_record(path: &Path, id: &str) -> anyhow::Result<Option<CutexSessionRecord>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(value) => value,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "invalid session database"
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            metadata.file_attributes() & 0x400 == 0,
            "linked session database"
        );
    }
    // An already-live WAL has SHM. Refuse instead of allowing SQLite to create
    // the shared-memory file when an offline WAL has no corresponding SHM.
    let wal = std::path::PathBuf::from(format!("{}-wal", path.display()));
    let shm = std::path::PathBuf::from(format!("{}-shm", path.display()));
    ensure!(
        !wal.exists() || shm.exists(),
        "session database has an offline WAL; open the host normally before previewing"
    );
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(Duration::from_millis(50))?;
    let row:Option<Option<String>>=connection.query_row("SELECT CASE WHEN length(record_json)<=2097152 THEN record_json ELSE NULL END FROM sessions WHERE cutex_session_id=?1",[id],|row|row.get(0)).optional()?;
    match row {
        None => Ok(None),
        Some(Some(raw)) => Ok(Some(serde_json::from_str(&raw)?)),
        Some(None) => anyhow::bail!("session record exceeds the preview limit"),
    }
}

#[derive(Clone)]
struct Facts {
    family: Option<String>,
    allow: Option<bool>,
}
fn claude_family(account: Option<&str>, registry: &ClaudeAccountsFile) -> String {
    let id = account.unwrap_or(claude_accounts::DEFAULT_ACCOUNT_ID);
    if id == claude_accounts::DEFAULT_ACCOUNT_ID {
        return "anthropic".into();
    }
    match registry.find(id) {
        Some(entry) => {
            crate::profiles::providers::family_for(
                &CliKind::Claude,
                None,
                entry.api.as_ref().and_then(|api| api.base_url.as_deref()),
            )
            .0
        }
        None => format!("custom:unresolved-claude-account:{id}"),
    }
}
fn facts(account: &StoredAccount, registry: &ClaudeAccountsFile) -> Facts {
    if account.cli_kind == CliKind::Claude {
        return Facts {
            family: Some(claude_family(account.claude_account.as_deref(), registry)),
            allow: Some(account.options.allow_session_switch.unwrap_or(false)),
        };
    }
    let cached = crate::profiles::fork_facts::get(account);
    Facts {
        family: cached.as_ref().map(|v| v.family.clone()),
        allow: account
            .options
            .allow_session_switch
            .or_else(|| cached.map(|v| v.allow_switch))
            .or_else(|| (account.source.as_deref() == Some("official")).then_some(false)),
    }
}
fn reason(code: &str, message: impl Into<String>) -> Value {
    json!({"code":code,"message":message.into()})
}
fn unavailable() -> Value {
    reason("profile_preview_unavailable","Refresh the host's profiles list, then preview again; credential-derived profile facts are unavailable or changed.")
}

fn source_facts(
    record: &CutexSessionRecord,
    store: &AccountsStore,
    registry: &ClaudeAccountsFile,
) -> (CliKind, Option<String>, Option<Value>) {
    let cli = if record.claude_session.is_some() {
        CliKind::Claude
    } else {
        CliKind::Codex
    };
    let profile = record
        .profile
        .as_deref()
        .and_then(|name| settings_api::find(store, name));
    let family = if cli == CliKind::Claude {
        Some(claude_family(
            claude_accounts::session_account_id(registry, record).as_deref(),
            registry,
        ))
    } else {
        profile.and_then(|p| facts(p, registry).family)
    };
    let refusal = if record.is_retired() {
        Some(reason(
            "session_fork_unsupported",
            "restore the archived session before forking it",
        ))
    } else if profile.is_none() {
        Some(reason(
            "profile_fork_disallowed",
            "the source has no resolvable profile; cannot verify provider compatibility",
        ))
    } else if profile.unwrap().cli_kind != cli {
        Some(reason(
            "profile_fork_disallowed",
            "the source profile no longer matches its runtime kind",
        ))
    } else if family.is_none() {
        Some(unavailable())
    } else {
        None
    };
    (cli, family, refusal)
}
fn target_reason(
    source: &StoredAccount,
    source_cli: &CliKind,
    source_family: Option<&str>,
    source_reason: Option<&Value>,
    target: &StoredAccount,
    target_facts: &Facts,
) -> Option<Value> {
    if let Some(refusal) = source_reason {
        return Some(refusal.clone());
    }
    if *source_cli != target.cli_kind {
        return Some(reason(
            "profile_switch_cli_mismatch",
            format!(
                "cannot fork a {source_cli} agent to a {} profile",
                target.cli_kind
            ),
        ));
    }
    let (Some(from), Some(to)) = (source_family, target_facts.family.as_deref()) else {
        return Some(unavailable());
    };
    if from != to {
        return Some(reason(
            "provider_family_mismatch",
            format!(
                "cannot fork from {} ({from}) to {} ({to}): different provider families",
                source.name, target.name
            ),
        ));
    }
    match target_facts.allow {
        Some(true)=>None,
        Some(false)=>Some(reason("profile_fork_disallowed",format!("profile {} does not allow receiving a fork; enable profile session switching first",target.name))),
        None=>Some(unavailable()),
    }
}

fn view(
    record: &CutexSessionRecord,
    store: &AccountsStore,
    registry: &ClaudeAccountsFile,
    query: &Query,
    size: crate::session::fork_preview_size::Estimate,
) -> Value {
    let (cli, family, source_reason) = source_facts(record, store, registry);
    let source = record
        .profile
        .as_deref()
        .and_then(|name| settings_api::find(store, name));
    let mut selected_reason = None;
    let mut selected_profile = None;
    let mut cold_cache = Some(false);
    let targets:Vec<_>=store.accounts.iter().map(|target|{
        let target_facts=facts(target,registry);
        let refusal=match source {Some(source)=>target_reason(source,&cli,family.as_deref(),source_reason.as_ref(),target,&target_facts),None=>source_reason.clone().or_else(||Some(unavailable()))};
        if query.profile.as_deref().is_some_and(|name|name==target.name||name==target.id) {
            selected_profile=Some(target.name.clone()); selected_reason=refusal.clone();
            cold_cache=if refusal.is_some(){None}else if cli==CliKind::Claude {
                Some(claude_accounts::session_account_id(registry,record).as_deref()!=Some(target.claude_account.as_deref().unwrap_or(claude_accounts::DEFAULT_ACCOUNT_ID)))
            }else{Some(record.profile.as_deref()!=Some(target.name.as_str()))};
        }
        json!({"id":target.id,"name":target.name,"cliKind":target.cli_kind.to_string(),"providerFamily":target_facts.family.as_deref().unwrap_or("unknown"),"eligible":refusal.is_none(),"reason":refusal})
    }).collect();
    if query.profile.is_some() && selected_profile.is_none() {
        selected_reason = Some(reason(
            "profile_not_found",
            "the requested fork profile does not exist",
        ));
        cold_cache = None;
    }
    if query.profile.is_none() {
        // Profile-omitted forks preserve the old behavior: no profile resolution
        // or switching opt-in is needed merely to keep the source profile.
        selected_reason = record.is_retired().then(|| {
            reason(
                "session_fork_unsupported",
                "restore the archived session before forking it",
            )
        });
    }
    let boundary = crate::session::fork::ForkOrigin {
        source_session_id: record.cutex_session_id.clone(),
        source_profile: record.profile.clone(),
        target_profile: selected_profile.clone().or(record.profile.clone()),
        history_bytes: size.bytes.unwrap_or(0),
        cold_cache: cold_cache.unwrap_or(false),
    }
    .boundary_note();
    json!({"contractVersion":2,"cutexSessionId":record.cutex_session_id,"afterTurnId":query.after_turn_id,"profile":selected_profile,"sourceProfile":record.profile,"sourceCliKind":cli.to_string(),"sourceProviderFamily":family,"targets":targets,"eligible":selected_reason.is_none(),"reason":selected_reason,"historyBytes":size.bytes,"historyBytesEstimate":if size.bytes.is_some(){"upperBound"}else{"unavailable"},"historyBytesMeaning":size.meaning,"coldCache":cold_cache,"boundaryNote":boundary})
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    if request.method != "GET" {
        return super::server::write_v2_error(
            stream,
            405,
            "Method Not Allowed",
            "method_not_allowed",
            "use GET for fork-preview",
            false,
            json!({}),
        );
    }
    let query = match parse_query(&request.path) {
        Ok(v) => v,
        Err(message) => {
            return super::server::write_v2_error(
                stream,
                400,
                "Bad Request",
                "invalid_request",
                &message,
                false,
                json!({}),
            )
        }
    };
    let id = session_id_from_path(&request.path).context("invalid fork preview route")?;
    let result = (|| -> anyhow::Result<Option<Value>> {
        let path = crate::session::store::session_database_path(
            &crate::session::store::cutex_sessions_path()?,
        );
        let Some(record) = read_record(&path, &id)? else {
            return Ok(None);
        };
        let store = settings_api::load_store()?;
        let registry = claude_accounts::load()?;
        let size =
            crate::session::fork_preview_size::estimate(&record, query.after_turn_id.as_deref());
        Ok(Some(view(&record, &store, &registry, &query, size)))
    })();
    match result {
        Ok(Some(body)) => write_json_response(stream, 200, "OK", &body),
        Ok(None) => super::server::write_v2_error(
            stream,
            404,
            "Not Found",
            "session_not_found",
            "no such session",
            false,
            json!({}),
        ),
        Err(error) => super::server::write_v2_error(
            stream,
            503,
            "Service Unavailable",
            "fork_preview_unavailable",
            &format!("fork preview unavailable: {error:#}"),
            true,
            json!({}),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn source() -> CutexSessionRecord {
        let mut record = CutexSessionRecord::new_at(
            "cutex.preview".into(),
            None,
            "host".into(),
            std::env::temp_dir().to_string_lossy().into_owned(),
            Some("source".into()),
            "2026-10-24T00:00:00Z".into(),
        )
        .unwrap();
        record.claude_session=Some(serde_json::from_value(json!({"deployment":{"python":"python","adapter_source":"adapter","claude_cli":"cli","claude_home":std::env::temp_dir().join("preview-default-account"),"tui_binary":"tui"},"thread_id":"native-thread","state_dir":"state","model":"sonnet"})).unwrap());
        record
    }
    fn store() -> AccountsStore {
        let mut source = crate::profiles::lifecycle::new_claude_account("source");
        source.options.allow_session_switch = Some(false);
        let mut target = crate::profiles::lifecycle::new_claude_account("other");
        target.options.allow_session_switch = Some(true);
        AccountsStore {
            accounts: vec![source, target],
            ..Default::default()
        }
    }
    fn size() -> crate::session::fork_preview_size::Estimate {
        crate::session::fork_preview_size::Estimate {
            bytes: Some(100),
            meaning: "Stat upper bound including later turns; membership not verified.".into(),
        }
    }
    #[test]
    fn query_paths_and_owner_class_are_strict() {
        assert_eq!(
            session_id_from_path("/v2/sessions/cutex%2Eone/fork-preview?profile=x").as_deref(),
            Some("cutex.one")
        );
        for path in [
            "/v2/sessions/a%2Fb/fork-preview",
            "/v2/sessions/a%5Cb/fork-preview",
            "/v2/sessions//fork-preview",
        ] {
            assert!(!is_path(path));
        }
        assert_eq!(
            parse_query("/v2/sessions/x/fork-preview?afterTurnId=&profile=").unwrap(),
            Query::default()
        );
        assert!(parse_query("?profile=a&profile=b").is_err());
        assert!(parse_query("?profile=a%0Ab").is_err());
        assert!(parse_query("?unknown=1").is_err());
        assert_eq!(
            super::super::access::classify("GET", "/v2/sessions/cutex.one/fork-preview"),
            Some(super::super::access::Class::OwnerOnly)
        );
    }
    #[test]
    fn source_opt_in_is_not_required_keep_current_does_not_opt_in() {
        let record = source();
        let profiles = store();
        let registry = ClaudeAccountsFile::default();
        let result = view(
            &record,
            &profiles,
            &registry,
            &Query {
                profile: Some("other".into()),
                after_turn_id: Some("earlier".into()),
            },
            size(),
        );
        assert_eq!(result["eligible"], true);
        assert_eq!(result["targets"][0]["eligible"], false);
        assert_eq!(result["sourceProviderFamily"], "anthropic");
        assert_eq!(result["coldCache"], false);
        assert!(result["boundaryNote"]
            .as_str()
            .unwrap()
            .contains("SOURCE session"));
        assert_eq!(result["historyBytesEstimate"], "upperBound");
        let schema: Value =
            serde_json::from_str(include_str!("schema/export/fork-preview.json")).unwrap();
        let validator = jsonschema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .build(&schema)
            .unwrap();
        assert!(validator.is_valid(&result), "{result}");
        assert_eq!(
            view(&record, &profiles, &registry, &Query::default(), size())["eligible"],
            true
        );
        assert_eq!(
            view(
                &record,
                &profiles,
                &registry,
                &Query {
                    profile: Some("source".into()),
                    after_turn_id: None
                },
                size()
            )["reason"]["code"],
            "profile_fork_disallowed"
        );
    }
    #[test]
    fn frozen_claude_account_wins_over_rebound_profile_and_same_name() {
        let record = source();
        let mut profiles = store();
        profiles.accounts[0].claude_account = Some("ca-relay".into());
        profiles.accounts[0].options.allow_session_switch = Some(true);
        let mut registry = ClaudeAccountsFile::default();
        registry.accounts.push(serde_json::from_value(json!({"id":"ca-relay","label":"Relay","kind":"api","createdAt":"today","api":{"baseUrl":"https://relay.example/anthropic"}})).unwrap());
        let result = view(
            &record,
            &profiles,
            &registry,
            &Query {
                profile: Some("source".into()),
                after_turn_id: None,
            },
            size(),
        );
        assert_eq!(result["sourceProviderFamily"], "anthropic");
        assert_eq!(
            result["targets"][0]["providerFamily"],
            "custom:relay.example"
        );
        assert_eq!(result["reason"]["code"], "provider_family_mismatch");
        profiles.accounts[1].cli_kind = CliKind::Codex;
        let result = view(
            &record,
            &profiles,
            &registry,
            &Query {
                profile: Some("other".into()),
                after_turn_id: None,
            },
            size(),
        );
        assert_eq!(result["reason"]["code"], "profile_switch_cli_mismatch");
    }

    #[test]
    fn frozen_named_api_account_supplies_family_even_when_profile_now_points_to_default() {
        let mut record = source();
        record
            .claude_session
            .as_mut()
            .unwrap()
            .deployment
            .claude_home = claude_accounts::account_home("ca-old").unwrap();
        let mut profiles = store();
        profiles.accounts[1].claude_account = Some("ca-old".into());
        let mut registry = ClaudeAccountsFile::default();
        registry.accounts.push(serde_json::from_value(json!({"id":"ca-old","label":"Original","kind":"api","createdAt":"today","api":{"baseUrl":"https://api.deepseek.com/anthropic"}})).unwrap());
        let result = view(
            &record,
            &profiles,
            &registry,
            &Query {
                profile: Some("other".into()),
                after_turn_id: None,
            },
            size(),
        );
        assert_eq!(result["sourceProviderFamily"], "deepseek");
        assert_eq!(result["eligible"], true);
        assert_eq!(result["coldCache"], false);
        assert_eq!(
            result["targets"][0]["reason"]["code"],
            "provider_family_mismatch"
        );
    }
    #[test]
    fn missing_codex_facts_fail_closed_without_opening_credentials() {
        let mut record = source();
        record.claude_session = None;
        let mut profiles = store();
        for p in &mut profiles.accounts {
            p.cli_kind = CliKind::Codex;
        }
        let result = view(
            &record,
            &profiles,
            &ClaudeAccountsFile::default(),
            &Query {
                profile: Some("other".into()),
                after_turn_id: None,
            },
            size(),
        );
        assert_eq!(result["sourceProviderFamily"], Value::Null);
        assert_eq!(result["reason"]["code"], "profile_preview_unavailable");
        assert_eq!(result["coldCache"], Value::Null);
    }

    #[test]
    fn public_profile_list_facts_enable_codex_preview_and_profile_changes_invalidate_them() {
        let mut record = source();
        record.claude_session = None;
        let mut profiles = store();
        for profile in &mut profiles.accounts {
            profile.cli_kind = CliKind::Codex;
            profile.source = Some("official".into());
            settings_api::profile_item(
                profile,
                &crate::profiles::model::CodezConfig::default(),
                false,
            );
        }
        let query = Query {
            profile: Some("other".into()),
            after_turn_id: None,
        };
        let result = view(
            &record,
            &profiles,
            &ClaudeAccountsFile::default(),
            &query,
            size(),
        );
        assert_eq!(result["eligible"], true);
        assert_eq!(result["sourceProviderFamily"], "openai-chatgpt");
        assert_eq!(result["coldCache"], true);
        profiles.accounts[1].name = "renamed".into();
        let result = view(
            &record,
            &profiles,
            &ClaudeAccountsFile::default(),
            &Query {
                profile: Some("renamed".into()),
                after_turn_id: None,
            },
            size(),
        );
        assert_eq!(result["reason"]["code"], "profile_preview_unavailable");
    }
    #[test]
    fn sqlite_record_read_has_no_migrations_or_history_writes() {
        let root = std::env::temp_dir().join(format!("fork-preview-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let db = root.join("sessions.sqlite3");
        let record = source();
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (cutex_session_id TEXT PRIMARY KEY, record_json TEXT)",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions VALUES (?1,?2)",
            rusqlite::params![
                record.cutex_session_id,
                serde_json::to_string(&record).unwrap()
            ],
        )
        .unwrap();
        drop(conn);
        let before = std::fs::read(&db).unwrap();
        assert!(read_record(&db, "missing").unwrap().is_none());
        assert_eq!(
            read_record(&db, &record.cutex_session_id).unwrap().unwrap(),
            record
        );
        assert_eq!(std::fs::read(&db).unwrap(), before);
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        assert!(read_record(&root.join("absent.sqlite3"), "missing")
            .unwrap()
            .is_none());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }
}
