//! Root-only `GET /v2/sessions/{id}/harness` (capability `agent-harness`):
//! the launch text cutex adds for one session, rendered now, for a desktop
//! preview. Codex agents get `# Cutex` (when enabled) before their
//! profile / role developer instructions, then `# File references` and
//! `# Memory`; Claude agents get the sections appended to the Claude Code
//! preset system prompt (AGENTS.md included, since Claude Code loads no
//! settings there). Facts are those a launch now would use; a running agent
//! keeps the text it was launched with.

use std::net::TcpStream;

use serde_json::{json, Value};

use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::session::model::{CutexSessionRecord, CutexSessionRuntimeBackend};

/// `Some(cutexSessionId)` for `/v2/sessions/{id}/harness`.
pub fn session_id_from_path(path: &str) -> Option<String> {
    let encoded = path.split('?').next()?.strip_prefix("/v2/sessions/")?.strip_suffix("/harness")?;
    if encoded.is_empty() || encoded.contains('/') {
        return None;
    }
    let decoded = super::attachments::percent_decode(encoded)?;
    (!decoded.is_empty() && !decoded.contains('/') && !decoded.chars().any(char::is_control)).then_some(decoded)
}

pub fn is_path(path: &str) -> bool {
    session_id_from_path(path).is_some()
}

/// The preview body for `record` with the current settings.
pub fn harness_view(record: &CutexSessionRecord, enabled: bool) -> Value {
    use crate::memory::harness;
    let claude = record.runtime_backend == CutexSessionRuntimeBackend::ClaudeSubscription;
    let rendered = harness::render_cutex_harness(record);
    let sections: Vec<(&'static str, String)> = if claude {
        harness::claude_system_append_sections(record)
    } else {
        let cwd = std::path::Path::new(&record.cwd);
        let memory = crate::memory::launch_memory_context_or_warn(&record.cutex_session_id);
        let root = memory.as_ref().map(|memory| memory.file_reference_root(cwd)).unwrap_or(cwd);
        let mut sections = Vec::new();
        if enabled {
            sections.push(("cutex", rendered.clone()));
        }
        sections.push(("fileReferences", crate::memory::render_file_citation_instructions(root)));
        if let Some(memory) = memory {
            sections.push(("memory", memory.instructions));
        }
        sections
    };
    let text = harness::join_sections(&sections);
    json!({
        "cutexSessionId": record.cutex_session_id,
        "runtime": if claude { "claude" } else { "codex" },
        "enabled": enabled,
        // The `# Cutex` section itself (also when disabled, for the preview).
        "harness": rendered,
        "harnessBytes": rendered.len(),
        // Where the launch text goes and what comes between cutex's sections.
        "placement": if claude { "claudePresetAppend" } else { "developerInstructions" },
        "sections": sections.iter().map(|(id, text)| json!({"id": id, "text": text, "bytes": text.len()})).collect::<Vec<_>>(),
        // Everything cutex adds, joined (Codex: the profile / role
        // developer instructions go between `cutex` and `fileReferences`).
        "text": text,
        "bytes": text.len(),
    })
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    if request.method != "GET" {
        return super::server::write_v2_error(stream, 405, "Method Not Allowed", "method_not_allowed",
            "use GET for /v2/sessions/{id}/harness", false, json!({}));
    }
    let Some(id) = session_id_from_path(&request.path) else {
        return super::server::write_v2_error(stream, 400, "Bad Request", "invalid_request",
            "an exact session id is required", false, json!({}));
    };
    let record = match crate::session::store::with_cutex_session_store(|store| store.sessions.get(&id).cloned()) {
        Ok(Some(record)) => record,
        Ok(None) => {
            return super::server::write_v2_error(stream, 404, "Not Found", "session_not_found",
                "no such session", false, json!({}));
        }
        Err(error) => {
            return super::server::write_v2_error(stream, 503, "Service Unavailable", "session_store_unavailable",
                &format!("session store: {error:#}"), true, json!({}));
        }
    };
    let enabled = crate::config::agent_harness::current().enabled;
    write_json_response(stream, 200, "OK", &harness_view(&record, enabled))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_paths() {
        assert_eq!(session_id_from_path("/v2/sessions/cutex.s1/harness").as_deref(), Some("cutex.s1"));
        assert_eq!(session_id_from_path("/v2/sessions/cutex.s1/harness?x=1").as_deref(), Some("cutex.s1"));
        assert_eq!(session_id_from_path("/v2/sessions/a%2Fb/harness"), None);
        assert_eq!(session_id_from_path("/v2/sessions//harness"), None);
        assert_eq!(session_id_from_path("/v2/sessions/a/b/harness"), None);
        assert!(!is_path("/v2/sessions/cutex.s1/attachments"));
    }

    #[test]
    fn view_for_a_codex_session_lists_cutex_sections_in_launch_order() {
        let mut record = CutexSessionRecord::new(
            "cutex.view-test".into(),
            None,
            "winbox".into(),
            std::env::temp_dir().to_string_lossy().into_owned(),
            None,
        )
        .unwrap();
        record.formal_agent_name = Some("Viewer".into());
        record.agent_enabled = true;
        let view = harness_view(&record, true);
        assert_eq!(view["runtime"], "codex");
        assert_eq!(view["placement"], "developerInstructions");
        let ids: Vec<&str> = view["sections"].as_array().unwrap().iter().map(|s| s["id"].as_str().unwrap()).collect();
        assert_eq!(&ids[..2], ["cutex", "fileReferences"]);
        let text = view["text"].as_str().unwrap();
        assert!(text.starts_with("# Cutex\n") && text.contains("- Name: Viewer (id cutex.view-test)"));
        assert_eq!(view["bytes"].as_u64().unwrap() as usize, text.len());

        let off = harness_view(&record, false);
        assert_eq!(off["enabled"], false);
        assert!(off["text"].as_str().unwrap().starts_with("# File references"));
        assert!(off["harness"].as_str().unwrap().starts_with("# Cutex"), "preview still renders it");
    }
}
