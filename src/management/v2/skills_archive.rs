//! Owner archive for user skills of the shared CODEX_HOME (capability
//! `skills-archive`). Root only, under the owner-only `/v2/codex` family.
//!
//! - `GET  /v2/codex/skills/archive` → `{ archives: [archiveRecord] }`
//! - `POST /v2/codex/skills/archive` `{ requestId, path }` → `{ archive, replayed }`
//!   `path` is a skill's `path` from `skills/list` (its SKILL.md or its dir).
//! - `POST /v2/codex/skills/archive/{id}/restore` `{ requestId }` → `{ archive, replayed }`
//!
//! Only user skills can be archived: an entry (directory or link) directly
//! or deeper inside `<CODEX_HOME>/skills` (not `.system`, the bundled
//! skills) or `~/.agents/skills`. System, admin, repo, plugin-provided and
//! extra-root skills answer 409 `skill_not_archivable` (details.reason).
//!
//! Archiving moves the entry (never copies or deletes) to
//! `<cutex home>/archive/skills/<id>/skill` and writes
//! `<id>/record.json`. Restore moves it back to the original path (409
//! `skill_name_taken` when something is there now) and keeps the record as
//! `archive/skills/.restored/<id>.json`. Both are idempotent by requestId.
//! Agents see the change on their next launch / resume, like skill toggles;
//! the warm codex config app-server is stopped so the next `skills/list`
//! re-scans.

use std::net::TcpStream;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::json;
use serde_json::Value;

use crate::http::server::write_json_response;
use crate::http::server::SimpleHttpRequest;

pub const PATH: &str = "/v2/codex/skills/archive";
pub const CAPABILITY: &str = "skills-archive";
const RECORD_FILE: &str = "record.json";
const ENTRY_NAME: &str = "skill";
const RESTORED_DIR: &str = ".restored";
const SYSTEM_DIR: &str = ".system";

pub const EXPORTED_SCHEMA: super::schema_export::ExportedSchema = super::schema_export::ExportedSchema {
    name: "skills-archive",
    title: "Skills archive",
    routes: &[
        "GET /v2/codex/skills/archive",
        "POST /v2/codex/skills/archive",
        "POST /v2/codex/skills/archive/{id}/restore",
    ],
    text: include_str!("schema/export/skills-archive.json"),
};

static LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    Collection,
    Restore(String),
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 160
        && !id.starts_with('.')
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

pub fn parse_path(path: &str) -> Option<Route> {
    if path == PATH {
        return Some(Route::Collection);
    }
    let id = path.strip_prefix("/v2/codex/skills/archive/")?.strip_suffix("/restore")?;
    valid_id(id).then(|| Route::Restore(id.to_string()))
}

pub fn is_path(path: &str) -> bool {
    path == PATH || path.starts_with("/v2/codex/skills/archive/")
}

/// Where user skills live and where archives go.
#[derive(Clone, Debug)]
pub struct Roots {
    /// The shared native home (`<CODEX_HOME>/skills` is a user root).
    pub codex_home: PathBuf,
    /// The OS user's home (`~/.agents/skills` is a user root).
    pub user_home: Option<PathBuf>,
    /// `<cutex home>/archive/skills`.
    pub archive_dir: PathBuf,
}

impl Roots {
    fn user_roots(&self) -> Vec<(&'static str, PathBuf)> {
        let mut roots = vec![("codexHome", self.codex_home.join("skills"))];
        if let Some(home) = &self.user_home {
            roots.push(("agentsHome", home.join(".agents").join("skills")));
        }
        roots
    }
}

#[derive(Debug, PartialEq)]
pub struct ArchiveError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
    pub details: Value,
}

fn err(status: u16, code: &'static str, message: impl Into<String>, details: Value) -> ArchiveError {
    ArchiveError { status, code, message: message.into(), details }
}

fn io_err(context: &str, error: std::io::Error) -> ArchiveError {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        return err(409, "skill_read_only", format!("{context}: {error}"), json!({}));
    }
    err(500, "skill_archive_io_failed", format!("{context}: {error}"), json!({}))
}

fn display(path: &Path) -> String {
    let text = path.to_string_lossy();
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_string()
}

fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

fn by_label() -> String {
    match crate::management::users::current_principal() {
        Some(crate::management::users::Principal::User { id, .. }) => format!("user:{id}"),
        _ => "owner".to_string(),
    }
}

/// name / description from SKILL.md front matter (best effort, display only).
fn front_matter(skill_md: &Path) -> (Option<String>, Option<String>) {
    let Ok(text) = std::fs::read_to_string(skill_md) else {
        return (None, None);
    };
    let text = text.trim_start_matches('\u{feff}');
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (None, None);
    }
    let (mut name, mut description) = (None, None);
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        let unquote = |v: &str| v.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
        if let Some(v) = line.strip_prefix("name:") {
            name = Some(unquote(v)).filter(|v| !v.is_empty());
        } else if let Some(v) = line.strip_prefix("description:") {
            description = Some(unquote(v)).filter(|v| !v.is_empty());
        }
    }
    (name, description.map(|d| d.chars().take(500).collect()))
}

/// The archivable entry for a skill path: (root kind, entry path, kind).
fn resolve_entry(roots: &Roots, path: &str) -> Result<(&'static str, PathBuf, &'static str), ArchiveError> {
    let raw = PathBuf::from(path);
    if path.is_empty() || !raw.is_absolute() || raw.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err(err(400, "invalid_request", "path must be an absolute skill path without ..", json!({})));
    }
    let is_md = raw.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("SKILL.md"));
    let dir = if is_md { raw.parent().map(Path::to_path_buf).unwrap_or(raw.clone()) } else { raw.clone() };
    let canon_dir = dir
        .canonicalize()
        .map_err(|_| err(404, "skill_not_found", "no skill at this path", json!({ "path": path })))?;
    if !canon_dir.join("SKILL.md").is_file() {
        return Err(err(404, "skill_not_found", "the directory has no SKILL.md", json!({ "path": path })));
    }
    let canon = |p: &Path| p.canonicalize().ok();
    let refuse = |reason: &str| {
        Err(err(
            409,
            "skill_not_archivable",
            match reason {
                "system" => "bundled system skills cannot be archived",
                "plugin" => "plugin-provided skills are managed with their plugin",
                _ => "only user skills in CODEX_HOME/skills or ~/.agents/skills can be archived",
            },
            json!({ "reason": reason, "path": path }),
        ))
    };
    if let Some(system) = canon(&roots.codex_home.join("skills").join(SYSTEM_DIR)) {
        if canon_dir.starts_with(&system) {
            return refuse("system");
        }
    }
    if let Some(plugins) = canon(&roots.codex_home.join("plugins")) {
        if canon_dir.starts_with(&plugins) {
            return refuse("plugin");
        }
    }
    for (kind, root) in roots.user_roots() {
        let Some(canon_root) = canon(&root) else { continue };
        if let Ok(rel) = canon_dir.strip_prefix(&canon_root) {
            if rel.as_os_str().is_empty() {
                return refuse("not_user_skill");
            }
            return Ok((kind, root.join(rel), "directory"));
        }
        // A link in the root pointing at the skill elsewhere: move the link.
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let is_link = entry.file_type().map(|t| t.is_symlink()).unwrap_or(false)
                    || std::fs::read_link(entry.path()).is_ok();
                if is_link && canon(&entry.path()).as_ref() == Some(&canon_dir) {
                    return Ok((kind, entry.path(), "link"));
                }
            }
        }
    }
    refuse("not_user_skill")
}

fn read_record(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn write_record(path: &Path, record: &Value) -> Result<(), ArchiveError> {
    // Unique temp name (concurrent writers never share `record.json.tmp`),
    // synced before the rename, Windows-safe replacement of an existing record.
    let data = serde_json::to_vec_pretty(record).expect("record serializes");
    crate::config::atomic::write_bytes_atomic(path, &data).map_err(|error| {
        let kind = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<std::io::Error>())
            .map_or(std::io::ErrorKind::Other, std::io::Error::kind);
        io_err("write archive record", std::io::Error::new(kind, format!("{error:#}")))
    })
}

fn view(record: &Value, archive_dir: &Path) -> Value {
    let mut view = record.clone();
    if let Some(id) = record["id"].as_str() {
        view["archivePath"] = json!(display(&archive_dir.join(id).join(ENTRY_NAME)));
    }
    view
}

/// Active archive records, newest first.
pub fn list(roots: &Roots) -> Result<Value, ArchiveError> {
    let mut records = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&roots.archive_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || !entry.path().is_dir() {
                continue;
            }
            if let Some(record) = read_record(&entry.path().join(RECORD_FILE)) {
                if record["id"].as_str() == Some(name.as_str()) {
                    records.push(view(&record, &roots.archive_dir));
                }
            }
        }
    }
    records.sort_by(|a, b| b["archivedAt"].as_str().cmp(&a["archivedAt"].as_str()));
    Ok(json!({ "archives": records }))
}

fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' })
        .take(100)
        .collect();
    let cleaned = cleaned.trim_start_matches('.').to_string();
    if cleaned.is_empty() { "skill".to_string() } else { cleaned }
}

fn find_by_request(roots: &Roots, request_id: &str, field: &str, restored: bool) -> Option<Value> {
    let dir = if restored { roots.archive_dir.join(RESTORED_DIR) } else { roots.archive_dir.clone() };
    std::fs::read_dir(dir).ok()?.flatten().find_map(|entry| {
        let path = if restored { entry.path() } else { entry.path().join(RECORD_FILE) };
        read_record(&path).filter(|record| record[field].as_str() == Some(request_id))
    })
}

pub fn archive(roots: &Roots, request_id: &str, path: &str) -> Result<Value, ArchiveError> {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(record) = find_by_request(roots, request_id, "requestId", false) {
        return Ok(json!({ "archive": view(&record, &roots.archive_dir), "replayed": true }));
    }
    let (root_kind, entry, entry_kind) = resolve_entry(roots, path)?;
    let name = entry.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let at = now();
    crate::management::client_tokens::ensure_private_dir(&roots.archive_dir)
        .map_err(|e| err(500, "skill_archive_io_failed", format!("{e:#}"), json!({})))?;
    let stamp = at.format("%Y%m%dT%H%M%SZ").to_string();
    let base = format!("{stamp}-{}", sanitize(&name));
    let (id, slot) = (1..1000)
        .map(|n| if n == 1 { base.clone() } else { format!("{base}-{n}") })
        .map(|id| (id.clone(), roots.archive_dir.join(id)))
        .find(|(_, slot)| std::fs::symlink_metadata(slot).is_err())
        .ok_or_else(|| err(500, "skill_archive_io_failed", "no free archive slot", json!({})))?;
    std::fs::create_dir(&slot).map_err(|e| io_err("create archive slot", e))?;
    let (skill_name, description) = front_matter(&entry.join("SKILL.md"));
    let link_target = std::fs::read_link(&entry).ok().map(|t| display(&t));
    let record = json!({
        "schemaVersion": 1,
        "id": id,
        "name": skill_name.unwrap_or_else(|| name.clone()),
        "dirName": name,
        "description": description,
        "scope": "user",
        "root": root_kind,
        "originalPath": display(&entry),
        "skillMdPath": display(&entry.join("SKILL.md")),
        "entryKind": entry_kind,
        "linkTarget": link_target,
        "archivedAt": at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "by": by_label(),
        "requestId": request_id,
    });
    let record_path = slot.join(RECORD_FILE);
    if let Err(error) = write_record(&record_path, &record) {
        let _ = std::fs::remove_dir(&slot);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&entry, slot.join(ENTRY_NAME)) {
        let _ = std::fs::remove_file(&record_path);
        let _ = std::fs::remove_dir(&slot);
        return Err(io_err("move the skill into the archive", error));
    }
    Ok(json!({ "archive": view(&record, &roots.archive_dir), "replayed": false }))
}

pub fn restore(roots: &Roots, request_id: &str, id: &str) -> Result<Value, ArchiveError> {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let slot = roots.archive_dir.join(id);
    let restored_path = roots.archive_dir.join(RESTORED_DIR).join(format!("{id}.json"));
    let Some(mut record) = read_record(&slot.join(RECORD_FILE)) else {
        return match read_record(&restored_path) {
            Some(done) if done["restoreRequestId"].as_str() == Some(request_id) => {
                Ok(json!({ "archive": done, "replayed": true }))
            }
            Some(_) => Err(err(409, "skill_already_restored", "this archive was already restored", json!({ "id": id }))),
            None => Err(err(404, "skill_archive_not_found", "no archived skill with this id", json!({ "id": id }))),
        };
    };
    let original = record["originalPath"].as_str().map(PathBuf::from).unwrap_or_default();
    let parent_ok = original.is_absolute()
        && !original.components().any(|c| matches!(c, std::path::Component::ParentDir))
        && roots.user_roots().iter().any(|(_, root)| original.starts_with(root) && original != *root);
    if !parent_ok {
        return Err(err(
            409,
            "skill_root_changed",
            "the original location is no longer a user skills root of this host's CODEX_HOME",
            json!({ "originalPath": record["originalPath"] }),
        ));
    }
    if std::fs::symlink_metadata(&original).is_ok() {
        return Err(err(
            409,
            "skill_name_taken",
            "a skill already exists at the original path; archive or rename it first",
            json!({ "originalPath": record["originalPath"] }),
        ));
    }
    let entry = slot.join(ENTRY_NAME);
    if std::fs::symlink_metadata(&entry).is_err() {
        return Err(err(409, "skill_archive_incomplete", "the archived skill entry is missing", json!({ "id": id })));
    }
    if let Some(parent) = original.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io_err("create the skills root", e))?;
    }
    std::fs::rename(&entry, &original).map_err(|e| io_err("move the skill back", e))?;
    record["restoredAt"] = json!(now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
    record["restoredBy"] = json!(by_label());
    record["restoreRequestId"] = json!(request_id);
    // The skill is back; keep the record as history. Failures here leave a
    // record without an entry (listed as incomplete), never a lost skill.
    let restored_dir = roots.archive_dir.join(RESTORED_DIR);
    if std::fs::create_dir_all(&restored_dir).is_ok() && write_record(&restored_path, &record).is_ok() {
        let _ = std::fs::remove_file(slot.join(RECORD_FILE));
        let _ = std::fs::remove_dir(&slot);
    }
    Ok(json!({ "archive": record, "replayed": false }))
}

// ---------------------------------------------------------------------------
// HTTP

fn live_roots() -> anyhow::Result<Roots> {
    let (_, _, codex_home) = super::codex_config::resolve_runtime()?;
    Ok(Roots {
        codex_home,
        user_home: crate::config::paths::home_dir(),
        archive_dir: crate::config::paths::config_dir()?.join("archive").join("skills"),
    })
}

fn body_object(body: &[u8], allowed: &[&str]) -> Result<serde_json::Map<String, Value>, ArchiveError> {
    let invalid = |m: String| err(400, "invalid_request", m, json!({}));
    let value: Value = serde_json::from_slice(body).map_err(|e| invalid(format!("invalid JSON request body: {e}")))?;
    let object = value.as_object().cloned().ok_or_else(|| invalid("request body must be a JSON object".into()))?;
    if let Some(key) = object.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(invalid(format!("unknown request field: {key}")));
    }
    match object.get("requestId").and_then(Value::as_str) {
        Some(id) if !id.is_empty() && id.chars().count() <= 256 => Ok(object),
        _ => Err(invalid("requestId must be a non-empty string of at most 256 characters".into())),
    }
}

/// Status + body for one request (separate from the socket for tests).
pub fn respond(roots: &Roots, method: &str, route: Route, body: &[u8]) -> Result<Value, ArchiveError> {
    let result = match (method, route) {
        ("GET", Route::Collection) => return list(roots),
        ("POST", Route::Collection) => {
            let object = body_object(body, &["requestId", "path"])?;
            let path = object.get("path").and_then(Value::as_str).unwrap_or_default();
            archive(roots, object["requestId"].as_str().unwrap_or_default(), path)
        }
        ("POST", Route::Restore(id)) => {
            let object = body_object(body, &["requestId"])?;
            restore(roots, object["requestId"].as_str().unwrap_or_default(), &id)
        }
        _ => Err(err(405, "method_not_allowed", "method not allowed for this skills archive route", json!({}))),
    };
    if result.as_ref().is_ok_and(|v| v["replayed"] == json!(false)) {
        super::codex_config::stop_warm_host();
    }
    result
}

/// Socket entry point; the caller has already verified the root credential.
pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let reason = |status: u16| match status {
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        500 => "Internal Server Error",
        _ => "Service Unavailable",
    };
    let Some(route) = parse_path(path) else {
        return super::server::write_v2_error(stream, 404, "Not Found", "not_found", "unknown skills archive route", false, json!({}));
    };
    let roots = match live_roots() {
        Ok(roots) => roots,
        Err(e) => {
            return super::server::write_v2_error(stream, 503, "Service Unavailable", "codex_home_unavailable",
                &format!("the shared CODEX_HOME could not be resolved: {e:#}"), true, json!({}))
        }
    };
    match respond(&roots, &request.method, route, &request.body) {
        Ok(body) => write_json_response(stream, 200, "OK", &body),
        Err(e) => super::server::write_v2_error(stream, e.status, reason(e.status), e.code, &e.message, e.status >= 500, e.details),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn setup() -> (Tmp, Roots) {
        let base = std::env::temp_dir().join(format!("cutex-skills-archive-{}", uuid::Uuid::new_v4().simple()));
        let roots = Roots {
            codex_home: base.join("codex-home"),
            user_home: Some(base.join("home")),
            archive_dir: base.join("cutex").join("archive").join("skills"),
        };
        std::fs::create_dir_all(&roots.codex_home).unwrap();
        (Tmp(base), roots)
    }

    fn skill(dir: &Path, name: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), format!("---\nname: {name}\ndescription: \"Does {name}\"\n---\n\n# {name}\n")).unwrap();
        std::fs::write(dir.join("helper.py"), "print(1)\n").unwrap();
        dir.join("SKILL.md")
    }

    fn s(path: &Path) -> String {
        path.to_string_lossy().to_string()
    }

    #[test]
    fn archive_list_restore_round_trip_with_idempotency() {
        let (_tmp, roots) = setup();
        let dir = roots.codex_home.join("skills").join("pdf");
        let md = skill(&dir, "pdf");

        let first = archive(&roots, "a1", &s(&md)).unwrap();
        assert_eq!(first["replayed"], false);
        let record = &first["archive"];
        assert_eq!(record["name"], "pdf");
        assert_eq!(record["description"], "Does pdf");
        assert_eq!(record["root"], "codexHome");
        assert_eq!(record["entryKind"], "directory");
        assert_eq!(record["by"], "owner");
        assert!(!dir.exists());
        let id = record["id"].as_str().unwrap().to_string();
        let moved = roots.archive_dir.join(&id).join("skill");
        assert!(moved.join("SKILL.md").is_file() && moved.join("helper.py").is_file());
        crate::management::v2::schema_export::assert_valid("skills-archive", "archiveResult", &first);

        // Same requestId: replayed, nothing moved again.
        let again = archive(&roots, "a1", &s(&md)).unwrap();
        assert_eq!(again["replayed"], true);
        assert_eq!(again["archive"]["id"], id.as_str());
        // A new requestId for the moved path: not found.
        assert_eq!(archive(&roots, "a2", &s(&md)).unwrap_err().code, "skill_not_found");

        let listed = list(&roots).unwrap();
        assert_eq!(listed["archives"].as_array().unwrap().len(), 1);
        crate::management::v2::schema_export::assert_valid("skills-archive", "archiveList", &listed);

        let restored = restore(&roots, "r1", &id).unwrap();
        assert_eq!(restored["replayed"], false);
        assert!(restored["archive"]["restoredAt"].is_string());
        crate::management::v2::schema_export::assert_valid("skills-archive", "archiveResult", &restored);
        assert!(dir.join("helper.py").is_file());
        assert!(!roots.archive_dir.join(&id).exists());
        assert!(roots.archive_dir.join(".restored").join(format!("{id}.json")).is_file());
        assert_eq!(list(&roots).unwrap()["archives"], json!([]));
        assert_eq!(restore(&roots, "r1", &id).unwrap()["replayed"], true);
        assert_eq!(restore(&roots, "r2", &id).unwrap_err().code, "skill_already_restored");
        assert_eq!(restore(&roots, "r3", "nope").unwrap_err().code, "skill_archive_not_found");
    }

    #[test]
    fn restore_refuses_a_taken_name_and_keeps_the_archive() {
        let (_tmp, roots) = setup();
        let dir = roots.user_home.as_ref().unwrap().join(".agents").join("skills").join("xlsx");
        skill(&dir, "xlsx");
        let archived = archive(&roots, "a", &s(&dir)).unwrap();
        assert_eq!(archived["archive"]["root"], "agentsHome");
        let id = archived["archive"]["id"].as_str().unwrap().to_string();
        skill(&dir, "xlsx-new");
        let error = restore(&roots, "r", &id).unwrap_err();
        assert_eq!((error.status, error.code), (409, "skill_name_taken"));
        assert!(roots.archive_dir.join(&id).join("skill").join("SKILL.md").is_file());
        assert_eq!(list(&roots).unwrap()["archives"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn non_user_skills_are_refused() {
        let (_tmp, roots) = setup();
        let system = skill(&roots.codex_home.join("skills").join(".system").join("skill-creator"), "skill-creator");
        let plugin = skill(&roots.codex_home.join("plugins").join("cache").join("gh").join("skills").join("review"), "review");
        let admin = skill(&roots.codex_home.parent().unwrap().join("etc").join("skills").join("ops"), "ops");
        for (path, reason) in [(system, "system"), (plugin, "plugin"), (admin, "not_user_skill")] {
            let error = archive(&roots, "x", &s(&path)).unwrap_err();
            assert_eq!((error.status, error.code), (409, "skill_not_archivable"));
            assert_eq!(error.details["reason"], reason);
            assert!(path.is_file());
        }
        // The root itself is not a skill entry.
        skill(&roots.codex_home.join("skills"), "root");
        assert_eq!(archive(&roots, "y", &s(&roots.codex_home.join("skills"))).unwrap_err().details["reason"], "not_user_skill");
        assert_eq!(archive(&roots, "z", "relative/SKILL.md").unwrap_err().status, 400);
        assert_eq!(archive(&roots, "z", &s(&roots.codex_home.join("skills").join("missing"))).unwrap_err().code, "skill_not_found");
        assert!(!roots.archive_dir.exists() || list(&roots).unwrap()["archives"] == json!([]));
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_skill_moves_the_link_not_its_target() {
        let (_tmp, roots) = setup();
        let target = roots.codex_home.parent().unwrap().join("src").join("notes");
        skill(&target, "notes");
        let skills = roots.codex_home.join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        std::os::unix::fs::symlink(&target, skills.join("notes")).unwrap();
        // skills/list reports the resolved path.
        let archived = archive(&roots, "a", &s(&target.join("SKILL.md"))).unwrap();
        assert_eq!(archived["archive"]["entryKind"], "link");
        assert!(target.join("SKILL.md").is_file());
        assert!(std::fs::symlink_metadata(skills.join("notes")).is_err());
        let id = archived["archive"]["id"].as_str().unwrap();
        restore(&roots, "r", id).unwrap();
        assert!(std::fs::symlink_metadata(skills.join("notes")).unwrap().file_type().is_symlink());
    }

    #[test]
    fn records_are_written_atomically_without_a_shared_temp_name() {
        use crate::config::atomic::test_hook::{self, Stage};
        let (_tmp, roots) = setup();
        let slot = roots.archive_dir.join("20261023T000000Z-pdf");
        std::fs::create_dir_all(&slot).unwrap();
        let path = slot.join(RECORD_FILE);
        write_record(&path, &json!({ "id": "old" })).unwrap();
        // A leftover of the old fixed temp name no longer blocks a write.
        std::fs::create_dir_all(path.with_extension("json.tmp")).unwrap();
        let stages = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = stages.clone();
        let target = path.clone();
        let guard = test_hook::observe(move |stage, tmp| {
            assert_eq!(tmp.parent(), target.parent());
            assert_eq!(read_record(&target).unwrap()["id"], "old", "the record is replaced only by the rename");
            seen.borrow_mut().push(stage);
        });
        write_record(&path, &json!({ "id": "new" })).unwrap();
        drop(guard);
        assert_eq!(*stages.borrow(), vec![Stage::Created, Stage::Written]);
        assert_eq!(read_record(&path).unwrap()["id"], "new");
        let names: Vec<String> = std::fs::read_dir(&slot)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp-"))
            .collect();
        assert!(names.is_empty(), "{names:?}");
    }

    #[test]
    fn routes_and_envelopes() {
        assert_eq!(parse_path(PATH), Some(Route::Collection));
        assert_eq!(parse_path("/v2/codex/skills/archive/20261015T010203Z-pdf/restore"), Some(Route::Restore("20261015T010203Z-pdf".into())));
        assert_eq!(parse_path("/v2/codex/skills/archive/.restored/restore"), None);
        assert_eq!(parse_path("/v2/codex/skills/archive/x"), None);
        let (_tmp, roots) = setup();
        assert_eq!(respond(&roots, "POST", Route::Collection, br#"{"path":"/x"}"#).unwrap_err().code, "invalid_request");
        assert_eq!(respond(&roots, "POST", Route::Collection, br#"{"requestId":"r","path":"/x","extra":1}"#).unwrap_err().code, "invalid_request");
        assert_eq!(respond(&roots, "DELETE", Route::Collection, b"").unwrap_err().status, 405);
        assert_eq!(respond(&roots, "GET", Route::Collection, b"").unwrap(), json!({ "archives": [] }));
    }
}
