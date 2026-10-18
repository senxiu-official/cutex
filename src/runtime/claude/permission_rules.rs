//! Persistent "always allow" rules of Claude agents (capability
//! `claude-permission-rules`, gap doc 2026-10-17 item 7 / Q10).
//!
//! Rules use Claude Code's permission rule syntax (`Bash(git status:*)`,
//! `Edit(src/**)`, `WebFetch(domain:example.com)`, `Read`) and are kept by
//! cutex per workspace (the agent's working directory), never in the
//! repository's `.claude/settings.json`. Storage: the cutex-owned sidecar
//! `~/.cutex/claude-permission-rules.json`; like `scoped-mcp.json` it does not
//! extend the shared `deny_unknown_fields` session / project stores.
//!
//! Every Claude session of a workspace gets the list as
//! `<state dir>/permission-rules.json` (`{"allow": [...]}`) before its adapter
//! starts and whenever the list changes; the adapter passes it to the official
//! runtime as flag settings (`settings.permissions.allow`) and, for a live
//! runtime, `claude/permissionRules/reload` applies the change at once when the
//! runtime accepts it (else at its next start). Adapters that do not declare
//! the `permissionRules` feature get no file.
//!
//! Rules are added by an approval answered with
//! `{"decision": {"acceptWithClaudeRules": {"rules": [...]}}}` (only rules the
//! adapter offered in that request's `claudePermissionRules`), and listed /
//! replaced through `GET|PUT /v2/sessions/{id}/claude-permission-rules`.

use std::collections::BTreeMap;
use std::fs;
use std::fs::OpenOptions;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::Context;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use serde_json::Value;

use super::ClaudeSession;
use crate::session::model::CutexSessionRecord;
use crate::session::model::CutexSessionRuntimeBackend;

pub const STORE_FILE: &str = "claude-permission-rules.json";
/// The per-session copy the adapter reads (`cute_claude.permission_rules`).
pub const SESSION_FILE: &str = "permission-rules.json";
/// Adapter `features.json` flag: it reads [`SESSION_FILE`] and serves
/// `claude/permissionRules/reload`.
pub const ADAPTER_FEATURE: &str = "permissionRules";
pub const MAX_RULES: usize = 256;
pub const MAX_RULE_CHARS: usize = 1024;
/// The approval decision that keeps rules (`cute_claude.approvals.RULE_DECISION`).
pub const RULE_DECISION: &str = "acceptWithClaudeRules";

static STORE_PROCESS_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRules {
    #[serde(default)]
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub allow: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RulesStore {
    #[serde(default = "store_version")]
    pub version: u32,
    #[serde(default)]
    pub workspaces: BTreeMap<String, WorkspaceRules>,
}

fn store_version() -> u32 {
    1
}

#[derive(Debug)]
pub enum UpdateError {
    Invalid(String),
    RevisionConflict { expected: u64, actual: u64 },
    Store(anyhow::Error),
}

/// Claude Code permission rule syntax: `Tool` or `Tool(specifier)`; tool
/// names are identifiers (`mcp__server__tool` included).
pub fn valid_rule(rule: &str) -> bool {
    if rule.is_empty() || rule.chars().count() > MAX_RULE_CHARS || rule.chars().any(|c| c == '\r' || c == '\n' || c == '\0') {
        return false;
    }
    let (name, rest) = match rule.split_once('(') {
        Some((name, rest)) => (name, Some(rest)),
        None => (rule, None),
    };
    let mut chars = name.chars();
    let name_ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    name_ok && rest.is_none_or(|rest| rest.len() > 1 && rest.ends_with(')'))
}

/// Validate and de-duplicate a full list (order kept).
pub fn normalized(rules: Vec<String>) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for rule in rules {
        let rule = rule.trim().to_string();
        if !valid_rule(&rule) {
            return Err(format!("invalid Claude permission rule: {rule:?} (use Tool or Tool(specifier))"));
        }
        if !out.contains(&rule) {
            out.push(rule);
        }
    }
    if out.len() > MAX_RULES {
        return Err(format!("at most {MAX_RULES} rules per workspace"));
    }
    Ok(out)
}

/// The store key of a session's workspace: its cwd without a Win32
/// verbatim prefix (`\\?\F:\x` and `F:\x` are one workspace; Claude records
/// before r245 stored the verbatim form) and without trailing separators.
pub fn workspace_key(cwd: &str) -> String {
    let cwd = crate::agent_management::strip_verbatim_prefix(cwd);
    let trimmed = cwd.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() || trimmed.ends_with(':') { cwd.clone() } else { trimmed.to_string() }
}

/// Entries written under a verbatim key by older releases are read under
/// their plain key (the next write stores them there). If both exist the
/// higher revision wins.
fn rekey_legacy_workspaces(store: &mut RulesStore) {
    let legacy: Vec<String> =
        store.workspaces.keys().filter(|key| workspace_key(key) != **key).cloned().collect();
    for key in legacy {
        let rules = store.workspaces.remove(&key).expect("listed key");
        let plain = workspace_key(&key);
        match store.workspaces.get(&plain) {
            Some(existing) if existing.revision >= rules.revision => {}
            _ => {
                store.workspaces.insert(plain, rules);
            }
        }
    }
}

pub fn store_path() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::config_dir()?.join(STORE_FILE))
}

pub fn load_store_at(path: &Path) -> anyhow::Result<RulesStore> {
    match fs::read_to_string(path) {
        Ok(text) => {
            let mut store: RulesStore = serde_json::from_str(&text)
                .with_context(|| format!("invalid Claude permission rules store: {}", path.display()))?;
            rekey_legacy_workspaces(&mut store);
            Ok(store)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(RulesStore { version: 1, ..RulesStore::default() }),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

pub fn rules_at(path: &Path, cwd: &str) -> anyhow::Result<WorkspaceRules> {
    Ok(load_store_at(path)?.workspaces.get(&workspace_key(cwd)).cloned().unwrap_or_default())
}

pub fn rules_for(cwd: &str) -> anyhow::Result<WorkspaceRules> {
    rules_at(&store_path()?, cwd)
}

/// Change one workspace's list under process + file locks. `change` gets the
/// current list and returns the new one (validated here).
pub fn update_at(
    path: &Path,
    cwd: &str,
    expected_revision: Option<u64>,
    change: impl FnOnce(Vec<String>) -> Vec<String>,
) -> Result<WorkspaceRules, UpdateError> {
    use fs2::FileExt;
    let _guard = STORE_PROCESS_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let lock_path = path.with_extension("lock");
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(|error| UpdateError::Store(error.into()))?;
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|error| UpdateError::Store(error.into()))?;
    lock.lock_exclusive().map_err(|error| UpdateError::Store(error.into()))?;
    let result = (|| {
        let mut store = load_store_at(path).map_err(UpdateError::Store)?;
        let key = workspace_key(cwd);
        let current = store.workspaces.get(&key).cloned().unwrap_or_default();
        if let Some(expected) = expected_revision {
            if expected != current.revision {
                return Err(UpdateError::RevisionConflict { expected, actual: current.revision });
            }
        }
        let allow = normalized(change(current.allow.clone())).map_err(UpdateError::Invalid)?;
        if allow == current.allow && expected_revision.is_none() {
            return Ok(current);
        }
        let next = WorkspaceRules {
            revision: current.revision + 1,
            updated_at: Some(chrono::Utc::now().to_rfc3339()),
            allow,
        };
        store.version = 1;
        // An emptied list keeps its entry so the revision stays monotonic.
        store.workspaces.insert(key, next.clone());
        crate::config::atomic::write_private_pretty_json_atomic(path, &store, "Claude permission rules")
            .map_err(UpdateError::Store)?;
        Ok(next)
    })();
    let _ = FileExt::unlock(&lock);
    result
}

pub fn replace(cwd: &str, allow: Vec<String>, expected_revision: Option<u64>) -> Result<WorkspaceRules, UpdateError> {
    let path = store_path().map_err(UpdateError::Store)?;
    update_at(&path, cwd, expected_revision, |_| allow)
}

pub fn add(cwd: &str, rules: &[String]) -> Result<WorkspaceRules, UpdateError> {
    let path = store_path().map_err(UpdateError::Store)?;
    update_at(&path, cwd, None, |mut current| {
        for rule in rules {
            if !current.contains(rule) {
                current.push(rule.clone());
            }
        }
        current
    })
}

/// Whether this session's adapter reads the rules file.
pub fn adapter_supports(session: &ClaudeSession) -> bool {
    super::adapter_features(&session.deployment.adapter_source)[ADAPTER_FEATURE] == true
}

/// Write the session's copy (`{"allow": [...]}`), read by the adapter at
/// every native start.
pub fn write_session_file(session: &ClaudeSession, allow: &[String]) -> anyhow::Result<()> {
    let path = session.state_dir.join(SESSION_FILE);
    crate::config::atomic::write_private_pretty_json_atomic(&path, &json!({ "allow": allow }), "Claude permission rules")
}

/// Launch: give a supporting adapter its workspace's current rules. A store
/// that cannot be read leaves no rules (the runtime keeps asking) rather than
/// failing the start.
pub fn prepare_launch(record: &CutexSessionRecord, session: &ClaudeSession) -> anyhow::Result<()> {
    if !adapter_supports(session) {
        return Ok(());
    }
    let allow = match rules_for(&record.cwd) {
        Ok(rules) => rules.allow,
        Err(error) => {
            eprintln!("claude permission rules: {error:#}; starting {} without persisted rules", record.cutex_session_id);
            Vec::new()
        }
    };
    write_session_file(session, &allow)
}

/// What happened to one session's copy after a change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSync {
    pub cutex_session_id: String,
    /// `live` (applied to the running runtime), `nextStart` (written; the
    /// runtime picks it up when it next starts), `unsupported` (adapter too old).
    pub status: &'static str,
}

/// Rewrite the copy of every local Claude session of `cwd`'s workspace and
/// ask the running ones to reload it.
pub fn sync_workspace(cwd: &str, allow: &[String]) -> Vec<SessionSync> {
    let key = workspace_key(cwd);
    let records: Vec<CutexSessionRecord> = crate::session::store::with_cutex_session_store(|store| {
        store
            .sessions
            .values()
            .filter(|record| {
                record.runtime_backend == CutexSessionRuntimeBackend::ClaudeSubscription
                    && record.claude_session.is_some()
                    && workspace_key(&record.cwd) == key
            })
            .cloned()
            .collect()
    })
    .unwrap_or_default();
    records.iter().map(|record| sync_session(record, allow)).collect()
}

fn sync_session(record: &CutexSessionRecord, allow: &[String]) -> SessionSync {
    let session = record.claude_session.as_ref().expect("filtered on claude_session");
    let id = record.cutex_session_id.clone();
    if !adapter_supports(session) {
        return SessionSync { cutex_session_id: id, status: "unsupported" };
    }
    if let Err(error) = write_session_file(session, allow) {
        eprintln!("claude permission rules: could not write the copy of {id}: {error:#}");
        return SessionSync { cutex_session_id: id, status: "nextStart" };
    }
    if record.runtime_pid.is_none() {
        return SessionSync { cutex_session_id: id, status: "nextStart" };
    }
    let applied = super::connect(session)
        .and_then(|(client, _)| Ok(client.handle().request("claude/permissionRules/reload", json!({ "threadId": session.thread_id }))?))
        .ok()
        .and_then(|result| result.get("applied").and_then(Value::as_str).map(str::to_string));
    SessionSync { cutex_session_id: id, status: if applied.as_deref() == Some("live") { "live" } else { "nextStart" } }
}

/// The rules an approval answer keeps: `decision.acceptWithClaudeRules.rules`,
/// only when every one was offered by the request (`claudePermissionRules`).
pub fn chosen_rules(request_params: &Value, result: &Value) -> Option<Vec<String>> {
    let chosen = result.pointer(&format!("/decision/{RULE_DECISION}/rules"))?.as_array()?;
    let offered = request_params.get("claudePermissionRules")?.as_array()?;
    let chosen: Vec<String> = chosen.iter().map(|rule| rule.as_str().map(str::to_string)).collect::<Option<_>>()?;
    (!chosen.is_empty() && chosen.iter().all(|rule| offered.iter().any(|o| o.as_str() == Some(rule)) && valid_rule(rule)))
        .then_some(chosen)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_syntax() {
        for good in ["Bash", "Bash(git status:*)", "Edit(src/**)", "WebFetch(domain:example.com)", "mcp__cutex__read_file"] {
            assert!(valid_rule(good), "{good}");
        }
        for bad in ["", "Bash(", "Bash()", "bad name", "Bash(x)\n", "(x)", "1Bash"] {
            assert!(!valid_rule(bad), "{bad:?}");
        }
        assert_eq!(normalized(vec![" Bash(ls:*) ".into(), "Bash(ls:*)".into(), "Edit".into()]).unwrap(), vec!["Bash(ls:*)", "Edit"]);
        assert!(normalized(vec!["no good".into()]).is_err());
    }

    #[test]
    fn verbatim_workspaces_share_the_plain_key_and_legacy_entries_are_found() {
        assert_eq!(workspace_key(r"\\?\F:\cutex-test\r244"), r"F:\cutex-test\r244");
        assert_eq!(workspace_key(r"\\?\F:\cutex-test\r244\"), r"F:\cutex-test\r244");
        let dir = std::env::temp_dir().join(format!("cutex-claude-rules-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(STORE_FILE);
        let legacy = json!({"version": 1, "workspaces": {
            r"\\?\F:\w": {"revision": 3, "allow": ["Read"]},
            r"\\?\G:\both": {"revision": 1, "allow": ["Old"]},
            r"G:\both": {"revision": 2, "allow": ["New"]}
        }});
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        // A new session (plain cwd) and an old one (verbatim cwd) see the same list.
        assert_eq!(rules_at(&path, r"F:\w").unwrap().allow, vec!["Read".to_string()]);
        assert_eq!(rules_at(&path, r"\\?\F:\w").unwrap().allow, vec!["Read".to_string()]);
        assert_eq!(rules_at(&path, r"G:\both").unwrap().allow, vec!["New".to_string()]);
        let next = update_at(&path, r"F:\w", Some(3), |mut allow| { allow.push("Edit".into()); allow }).unwrap();
        assert_eq!(next.revision, 4);
        let stored = load_store_at(&path).unwrap();
        assert_eq!(stored.workspaces.keys().cloned().collect::<Vec<_>>(), vec![r"F:\w".to_string(), r"G:\both".to_string()]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn workspace_keys_ignore_trailing_separators() {
        assert_eq!(workspace_key("/work/repo/"), "/work/repo");
        assert_eq!(workspace_key("/"), "/");
        assert_eq!(workspace_key(r"C:\"), r"C:\");
        assert_eq!(workspace_key(r"D:\repo\"), r"D:\repo");
    }

    #[test]
    fn store_updates_are_cas_fenced_and_per_workspace() {
        let dir = std::env::temp_dir().join(format!("cutex-claude-rules-{}", uuid::Uuid::new_v4()));
        let path = dir.join(STORE_FILE);
        assert_eq!(rules_at(&path, "/w").unwrap(), WorkspaceRules::default());
        let first = update_at(&path, "/w/", None, |_| vec!["Bash(ls:*)".into()]).unwrap();
        assert_eq!((first.revision, first.allow.clone()), (1, vec!["Bash(ls:*)".to_string()]));
        // Same list without a revision: unchanged, no new revision.
        assert_eq!(update_at(&path, "/w", None, |current| current).unwrap().revision, 1);
        assert!(matches!(
            update_at(&path, "/w", Some(0), |_| vec![]),
            Err(UpdateError::RevisionConflict { expected: 0, actual: 1 })
        ));
        assert!(matches!(update_at(&path, "/w", None, |_| vec!["bad name".into()]), Err(UpdateError::Invalid(_))));
        let emptied = update_at(&path, "/w", Some(1), |_| vec![]).unwrap();
        assert_eq!((emptied.revision, emptied.allow.len()), (2, 0));
        assert!(rules_at(&path, "/other").unwrap().allow.is_empty());
        let stored = load_store_at(&path).unwrap();
        assert_eq!(stored.workspaces.keys().collect::<Vec<_>>(), vec!["/w"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_offered_rules_are_kept_from_an_answer() {
        let params = json!({"claudePermissionRules": ["Bash(git status:*)"]});
        let answer = |rules: Value| json!({"decision": {RULE_DECISION: {"rules": rules}}});
        assert_eq!(chosen_rules(&params, &answer(json!(["Bash(git status:*)"]))), Some(vec!["Bash(git status:*)".to_string()]));
        assert_eq!(chosen_rules(&params, &answer(json!(["Bash(*)"]))), None);
        assert_eq!(chosen_rules(&params, &answer(json!([]))), None);
        assert_eq!(chosen_rules(&json!({}), &answer(json!(["Bash(git status:*)"]))), None);
        assert_eq!(chosen_rules(&params, &json!({"decision": "accept"})), None);
    }
}
