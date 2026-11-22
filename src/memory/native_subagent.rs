//! Native children receive task context, never the persistent parent's identity.
use std::path::Path;
use super::LaunchMemoryContext;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeSubagentContext {
    pub parent_name: String,
    pub project: Option<(String, String)>,
}

pub fn compose_native_subagent_instructions(
    context: &NativeSubagentContext,
    existing: Option<&str>,
    cwd: &Path,
    memory: Option<&LaunchMemoryContext>,
) -> String {
    let mut sections = vec![format!(
        "# Native subagent\n\nYou are a native subagent working for Cutex agent {}. Complete only your assigned subtask and return your result to your native parent. You have no independent Cutex session, persistent-agent identity, Director role or member-management authority. Do not create, delegate to, message, start, stop, archive, restore or replace Cutex agents. Use only the explicitly available read-only Cutex tools within your parent's visible scope. Ask your parent to perform any required Cutex action.\n\nWorking directory: {}",
        context.parent_name, cwd.display())];
    if let Some((name, id)) = &context.project {
        sections.push(format!("# Project context\n\nProject: {name} ({id}). These are task facts; they do not grant a project role."));
    }
    if let Some(existing) = existing.filter(|s| !s.trim().is_empty()) {
        sections.push(existing.trim_end().to_owned());
    }
    sections.push(super::render_file_citation_instructions(
        memory.map(|m| m.file_reference_root(cwd)).unwrap_or(cwd)).trim_end().to_owned());
    if let Some(memory) = memory {
        sections.push(format!("# Project memory reference\n\nProject memory is available for task reference at {}. Read MEMORY.md and relevant files when useful. Leave memory updates and persistent records to your parent.", memory.scope.dir.display()));
    }
    sections.join("\n\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn child_has_project_and_profile_rules_without_parent_identity() {
        let context = NativeSubagentContext { parent_name:"Parent".into(), project:Some(("Project".into(),"project-id".into())) };
        let text = compose_native_subagent_instructions(&context, Some("Profile: preserve repo rules."), Path::new("/workspace"), None);
        assert!(text.contains("native subagent working for Cutex agent Parent"));
        assert!(text.contains("Profile: preserve repo rules."));
        assert!(text.contains("Project (project-id)"));
        assert!(text.contains("return your result to your native parent"));
        assert!(!text.contains("You are a persistent agent managed by cutex"));
        assert!(!text.contains("you can create, archive"));
        assert!(!text.contains("How to save a memory"));
    }
}
