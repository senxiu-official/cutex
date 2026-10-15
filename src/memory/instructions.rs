//! `# Memory` developer-instruction section injected at Agent launch.

use std::path::Path;
use std::path::PathBuf;

use super::ProjectMemoryScope;

pub const MEMORY_INDEX_MAX_LINES: usize = 200;
pub const MEMORY_INDEX_MAX_BYTES: usize = 25 * 1024;

/// Launch-time memory context for one project Agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchMemoryContext {
    pub scope: ProjectMemoryScope,
    /// The rendered `# Memory` section (instructions + current index).
    pub instructions: String,
    /// The project's Human-set `workspace_root`, when set. File references
    /// are relative to it instead of the Agent's launch cwd.
    pub project_workspace_root: Option<PathBuf>,
}

impl LaunchMemoryContext {
    pub fn writable_root(&self) -> &Path {
        &self.scope.dir
    }

    /// Root for `# File references`: the project's workspace root when set,
    /// else the launch cwd.
    pub fn file_reference_root<'a>(&'a self, cwd: &'a Path) -> &'a Path {
        self.project_workspace_root.as_deref().unwrap_or(cwd)
    }

    /// Append the memory section to existing developer instructions.
    pub fn merge_developer_instructions(&self, existing: Option<String>) -> String {
        match existing.filter(|value| !value.trim().is_empty()) {
            Some(existing) => format!("{}\n\n{}", existing.trim_end(), self.instructions),
            None => self.instructions.clone(),
        }
    }

    /// Runtime workspace roots for a project Agent: its cwd plus the memory
    /// directory, so `:workspace` / `workspace-write` sandboxes can write
    /// memory files.
    pub fn runtime_workspace_roots(&self, cwd: &Path) -> Vec<PathBuf> {
        let mut roots = vec![cwd.to_path_buf()];
        if self.scope.dir != cwd {
            roots.push(self.scope.dir.clone());
        }
        roots
    }
}

/// Resolve the memory context for a session at launch. Returns `Ok(None)` for
/// independent Agents. Creates the memory directory so the Agent can write.
pub fn launch_memory_context(
    cutex_session_id: &str,
) -> anyhow::Result<Option<LaunchMemoryContext>> {
    let snapshot = super::load_agent_management_snapshot()?;
    let Some(scope) = super::memory_scope_for_session(&snapshot, cutex_session_id)? else {
        return Ok(None);
    };
    let store = super::MemoryStore::new(scope.dir.clone());
    store.ensure_dir()?;
    let index = store
        .read_index()
        .map_err(|error| anyhow::anyhow!("{error}"))?
        .map(|document| document.content);
    let instructions = render_memory_instructions(&scope, index.as_deref());
    let project_workspace_root = snapshot
        .project_states
        .get(&scope.project_id)
        .and_then(|state| state.workspace_root.as_deref())
        .filter(|root| !root.trim().is_empty())
        .map(PathBuf::from);
    Ok(Some(LaunchMemoryContext {
        scope,
        instructions,
        project_workspace_root,
    }))
}

/// Best-effort wrapper used by launch paths: memory must never block a launch.
pub fn launch_memory_context_or_warn(cutex_session_id: &str) -> Option<LaunchMemoryContext> {
    match launch_memory_context(cutex_session_id) {
        Ok(context) => context,
        Err(error) => {
            eprintln!(
                "project memory unavailable for cutex session {cutex_session_id}; launching without it: {error:#}"
            );
            None
        }
    }
}

/// Short file-citation section injected for every managed Agent (desktop
/// renders these links as clickable file references; it resolves absolute
/// paths and paths relative to the workspace root alike).
pub fn render_file_citation_instructions(workspace_root: &Path) -> String {
    format!(
        "# File references\n\n\
         When you mention a file, cite it as a markdown link whose target is its absolute path or its path relative to the workspace root `{}`, optionally followed by `:line`, for example [quota.rs](src/profiles/quota.rs:42).\n\
         Use forward slashes in relative targets; wrap a target that contains spaces in angle brackets.\n",
        workspace_root.display()
    )
}

/// Developer instructions and runtime roots cutex adds at Agent launch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchInstructions {
    pub developer_instructions: String,
    /// `Some` only for project members (cwd + memory dir).
    pub runtime_workspace_roots: Option<Vec<PathBuf>>,
}

/// Compose `# Cutex` (when enabled) + `existing` + `# File references` +
/// (project members only) `# Memory`. `workspace_root` is the Agent's launch
/// cwd; file references use the project's `workspace_root` instead when the
/// Agent is a project member and the project has one set.
pub fn compose_launch_instructions(
    harness: Option<&str>,
    existing: Option<String>,
    workspace_root: &Path,
    memory: Option<&LaunchMemoryContext>,
) -> LaunchInstructions {
    let mut sections = Vec::new();
    if let Some(harness) = harness.filter(|value| !value.trim().is_empty()) {
        sections.push(harness.trim_end().to_string());
    }
    if let Some(existing) = existing.filter(|value| !value.trim().is_empty()) {
        sections.push(existing.trim_end().to_string());
    }
    let citation_root = memory
        .map(|memory| memory.file_reference_root(workspace_root))
        .unwrap_or(workspace_root);
    sections.push(
        render_file_citation_instructions(citation_root)
            .trim_end()
            .to_string(),
    );
    if let Some(memory) = memory {
        sections.push(memory.instructions.trim_end().to_string());
    }
    let mut developer_instructions = sections.join("\n\n");
    developer_instructions.push('\n');
    LaunchInstructions {
        developer_instructions,
        runtime_workspace_roots: memory
            .map(|memory| memory.runtime_workspace_roots(workspace_root)),
    }
}

/// Launch-time additions carried (unserialized) on native thread start/resume
/// params. The runtime manager resolves them against the app-server's own
/// effective config (`config/read`) right before sending the request, so the
/// sections are appended to, never substituted for, the thread's
/// developer instructions and writable roots.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LaunchAugmentation {
    /// The `# Cutex` section (`None` when `agent_harness.enabled` is off).
    pub harness: Option<String>,
    pub memory: Option<LaunchMemoryContext>,
}

/// Build the augmentation for one session (memory lookup is best effort).
pub fn launch_augmentation_for_session(
    record: &crate::session::model::CutexSessionRecord,
) -> LaunchAugmentation {
    LaunchAugmentation {
        harness: super::harness::launch_harness(record),
        memory: launch_memory_context_or_warn(&record.cutex_session_id),
    }
}

/// Mark a native `thread/start` for launch augmentation.
pub fn attach_launch_augmentation_to_thread_start(
    params: &mut crate::app_server::commands::ThreadStartParams,
    record: &crate::session::model::CutexSessionRecord,
) {
    params.launch_augmentation = Some(launch_augmentation_for_session(record));
}

/// Mark a native `thread/resume` for launch augmentation. A resumed thread
/// with prior turns only re-renders developer instructions after compaction;
/// new/empty threads see them on their first turn.
pub fn attach_launch_augmentation_to_thread_resume(
    params: &mut crate::app_server::commands::ThreadResumeParams,
    record: &crate::session::model::CutexSessionRecord,
) {
    params.launch_augmentation = Some(launch_augmentation_for_session(record));
}

/// Result of resolving an augmentation against the effective config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedLaunchAugmentation {
    /// Value to send as `developerInstructions` (the explicit param when
    /// injection was skipped).
    pub developer_instructions: Option<String>,
    /// `Some` only when the configured roots were readable (members only).
    pub runtime_workspace_roots: Option<Vec<PathBuf>>,
    /// True when the instructions could not be resolved and were left alone.
    pub instructions_skipped: bool,
}

fn config_string(config: &serde_json::Value, key: &str) -> Option<String> {
    config
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

/// Writable roots the thread would get without an override, or `None` when
/// they cannot be determined from the effective config.
///
/// cute-codex seeds workspace roots as `[cwd] + --add-dir +
/// sandbox_workspace_write.writable_roots`, the last only while no permission
/// profile is selected (neither the `permissions` param nor
/// `default_permissions`). Profile-defined `[permissions.*].workspace_roots`
/// are kept separately by codex and are unaffected by the override.
pub fn configured_workspace_roots(
    cwd: &Path,
    permissions_param: Option<&str>,
    effective_config: &serde_json::Value,
) -> Option<Vec<PathBuf>> {
    let mut roots = vec![cwd.to_path_buf()];
    let profile_selected = permissions_param.is_some_and(|value| !value.trim().is_empty())
        || effective_config
            .get("default_permissions")
            .is_some_and(|value| !value.is_null());
    if !profile_selected {
        match effective_config
            .get("sandbox_workspace_write")
            .and_then(|value| value.get("writable_roots"))
        {
            None | Some(serde_json::Value::Null) => {}
            Some(serde_json::Value::Array(values)) => {
                for value in values {
                    let path = PathBuf::from(value.as_str()?);
                    if !path.is_absolute() {
                        return None;
                    }
                    roots.push(path);
                }
            }
            Some(_) => return None,
        }
    }
    Some(roots)
}

/// Resolve the augmentation. `explicit` is the `developerInstructions` param
/// cutex already sends (e.g. a Release role package), which cute-codex prefers
/// over config; otherwise the config `developer_instructions` is the
/// effective value. `effective_config` is the `config` object from
/// `config/read {cwd}`, `None` when that read failed.
pub fn resolve_launch_augmentation(
    augmentation: &LaunchAugmentation,
    explicit: Option<String>,
    cwd: Option<&Path>,
    permissions_param: Option<&str>,
    effective_config: Option<&serde_json::Value>,
) -> ResolvedLaunchAugmentation {
    let skipped = |explicit| ResolvedLaunchAugmentation {
        developer_instructions: explicit,
        runtime_workspace_roots: None,
        instructions_skipped: true,
    };
    let Some(cwd) = cwd else {
        return skipped(explicit);
    };
    let existing = match (explicit, effective_config) {
        (Some(explicit), _) => Some(explicit),
        (None, Some(config)) => config_string(config, "developer_instructions"),
        (None, None) => return skipped(None),
    };
    let composed = compose_launch_instructions(
        augmentation.harness.as_deref(),
        existing,
        cwd,
        augmentation.memory.as_ref(),
    );
    let runtime_workspace_roots = match (augmentation.memory.as_ref(), effective_config) {
        (Some(memory), Some(config)) => configured_workspace_roots(cwd, permissions_param, config)
            .map(|mut roots| {
                roots.push(memory.scope.dir.clone());
                let mut seen = std::collections::HashSet::new();
                roots.retain(|root| seen.insert(root.clone()));
                roots
            }),
        _ => None,
    };
    ResolvedLaunchAugmentation {
        developer_instructions: Some(composed.developer_instructions),
        runtime_workspace_roots,
        instructions_skipped: false,
    }
}

/// Truncate the index to `MEMORY_INDEX_MAX_LINES` lines and
/// `MEMORY_INDEX_MAX_BYTES` bytes (on a line boundary). Returns the kept text
/// and whether anything was dropped.
pub fn truncate_memory_index(index: &str) -> (String, bool) {
    let total_lines = index.lines().count();
    let mut kept = String::new();
    let mut truncated = false;
    for (count, line) in index.lines().enumerate() {
        if count >= MEMORY_INDEX_MAX_LINES || kept.len() + line.len() + 1 > MEMORY_INDEX_MAX_BYTES {
            truncated = true;
            break;
        }
        kept.push_str(line);
        kept.push('\n');
    }
    if !truncated && kept.lines().count() < total_lines {
        truncated = true;
    }
    (kept, truncated)
}

pub fn render_memory_instructions(scope: &ProjectMemoryScope, index: Option<&str>) -> String {
    let dir = scope.dir.display();
    let project = scope.project_id.as_str();
    let index_path = scope.dir.join(super::MEMORY_INDEX_FILE);
    let index_path = index_path.display();
    let mut out = format!(
        r#"# Memory

You have a persistent, file-based memory shared by every agent in project `{project}` (and across model providers). It lives in this directory:

`{dir}`

Use it to remember what will matter in future conversations: who the user is and how they like to work, feedback they have given, decisions and context about this project that are not in the code, and where to find things outside the repository.

## How to save a memory

Write one fact per markdown file in that directory with your normal file tools. File names use only lowercase letters, digits, `.`, `_` and `-`, and end in `.md` (for example `feedback-integration-tests.md`). Every file starts with YAML frontmatter:

```markdown
---
name: kebab-case-slug
description: One line saying what this memory is, used later to judge relevance
metadata:
  type: user | feedback | project | reference
---

The fact itself, written so another agent can use it without this conversation.
For feedback and project memories add:
**Why:** the reason or incident behind it.
**How to apply:** when and how to use it.
Link related memories with [[other-memory-name]].
```

Types:
- `user`: the user's role, goals, expertise, preferences and communication style.
- `feedback`: corrections or confirmations of how you should work (both what to avoid and what worked well).
- `project`: goals, decisions, constraints, deadlines and who does what, when not derivable from the code or git history.
- `reference`: pointers to external resources (dashboards, documents, tickets, hosts, channels).

Then add or update exactly one line for it in the index `{index_path}`:

`- [Title](file-name.md) — one-line hook`

The index holds no memory content of its own. It is loaded into every agent's context at launch and truncated after {MEMORY_INDEX_MAX_LINES} lines, so keep each line short.

## Rules

- Save when the user asks you to remember something, or when you learn something non-obvious that a future agent in this project would need. If the user asks you to forget something, delete the file and its index line.
- Check the index first and update an existing memory instead of creating a duplicate. Fix or delete memories that turn out to be wrong or outdated, and keep the index in sync.
- Convert relative dates to absolute dates (YYYY-MM-DD).
- Do not save: anything derivable from the code, git history or AGENTS.md; temporary task state or the progress of the current conversation; secrets, credentials, tokens or personal data the user did not ask you to keep.
- Memories can be stale. Before acting on a memory that names specific files, functions or flags, check that they still exist.
- Only write memory inside the directory above. It is private to this project's agents.

## Current MEMORY.md
"#
    );
    match index.filter(|index| !index.trim().is_empty()) {
        None => out.push_str("\n(empty: no memories have been saved for this project yet)\n"),
        Some(index) => {
            let (kept, truncated) = truncate_memory_index(index);
            out.push('\n');
            out.push_str(&kept);
            if truncated {
                out.push_str(&format!(
                    "\n[MEMORY.md truncated to {MEMORY_INDEX_MAX_LINES} lines / {} KB; read the full file at {index_path} and keep the index concise]\n",
                    MEMORY_INDEX_MAX_BYTES / 1024
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_management::ProjectId;

    fn scope() -> ProjectMemoryScope {
        ProjectMemoryScope {
            project_id: ProjectId::new("proj-1").unwrap(),
            dir: PathBuf::from("/home/u/.cutex/projects/proj-1-abc/memory"),
        }
    }

    #[test]
    fn renders_instructions_with_dir_format_and_index() {
        let text = render_memory_instructions(
            &scope(),
            Some("- [test-style](test-style.md) — prefer real DB\n"),
        );
        assert!(text.starts_with("# Memory\n"));
        assert!(text.contains("`/home/u/.cutex/projects/proj-1-abc/memory`"));
        assert!(text.contains("project `proj-1`"));
        assert!(text.contains("metadata:\n  type: user | feedback | project | reference"));
        assert!(text.contains("- [Title](file-name.md) — one-line hook"));
        assert!(text
            .contains("## Current MEMORY.md\n\n- [test-style](test-style.md) — prefer real DB\n"));
        assert!(!text.contains("[MEMORY.md truncated"));
    }

    #[test]
    fn renders_empty_index_note() {
        for index in [None, Some(""), Some("  \n")] {
            let text = render_memory_instructions(&scope(), index);
            assert!(text.ends_with("(empty: no memories have been saved for this project yet)\n"));
        }
    }

    #[test]
    fn truncates_index_by_lines_and_bytes() {
        let many: String = (0..250)
            .map(|i| format!("- [m{i}](m{i}.md) — hook\n"))
            .collect();
        let (kept, truncated) = truncate_memory_index(&many);
        assert!(truncated);
        assert_eq!(kept.lines().count(), MEMORY_INDEX_MAX_LINES);
        let text = render_memory_instructions(&scope(), Some(&many));
        assert!(text.contains("[MEMORY.md truncated to 200 lines / 25 KB"));
        assert!(!text.contains("(m200.md)"));

        let wide: String = (0..50)
            .map(|i| format!("- [m{i}](m{i}.md) — {}\n", "x".repeat(1000)))
            .collect();
        let (kept, truncated) = truncate_memory_index(&wide);
        assert!(truncated);
        assert!(kept.len() <= MEMORY_INDEX_MAX_BYTES);
        assert!(kept.ends_with('\n'));

        let (kept, truncated) = truncate_memory_index("- a\n- b\n");
        assert_eq!((kept.as_str(), truncated), ("- a\n- b\n", false));
    }

    #[test]
    fn renders_file_citation_section() {
        let text = render_file_citation_instructions(Path::new("/work/repo"));
        assert_eq!(
            text,
            "# File references\n\nWhen you mention a file, cite it as a markdown link whose target is its absolute path or its path relative to the workspace root `/work/repo`, optionally followed by `:line`, for example [quota.rs](src/profiles/quota.rs:42).\nUse forward slashes in relative targets; wrap a target that contains spaces in angle brackets.\n"
        );
        assert!(text.lines().count() <= 5);
    }

    #[test]
    fn composes_citation_for_everyone_and_memory_for_members() {
        let independent = compose_launch_instructions(None, None, Path::new("/work"), None);
        assert!(independent
            .developer_instructions
            .starts_with("# File references\n"));
        assert!(!independent.developer_instructions.contains("# Memory"));
        assert_eq!(independent.runtime_workspace_roots, None);

        let memory = LaunchMemoryContext {
            scope: scope(),
            instructions: render_memory_instructions(&scope(), None),
            project_workspace_root: None,
        };
        let member = compose_launch_instructions(
            Some("# Cutex\n\nharness\n"),
            Some("verified role\n".to_string()),
            Path::new("/work"),
            Some(&memory),
        );
        let text = &member.developer_instructions;
        assert!(text.starts_with("# Cutex\n\nharness\n\nverified role\n\n# File references"), "{text}");
        let role = text.find("verified role").unwrap();
        let cite = text.find("# File references").unwrap();
        let mem = text.find("# Memory").unwrap();
        assert!(role < cite && cite < mem);
        assert!(text.ends_with("yet)\n"));
        assert_eq!(
            member.runtime_workspace_roots,
            Some(vec![PathBuf::from("/work"), scope().dir])
        );
        assert!(text.contains("relative to the workspace root `/work`"));
    }

    #[test]
    fn file_references_use_project_workspace_root_when_set() {
        let memory = LaunchMemoryContext {
            scope: scope(),
            instructions: render_memory_instructions(&scope(), None),
            project_workspace_root: Some(PathBuf::from("/projects/alpha")),
        };
        let member =
            compose_launch_instructions(None, None, Path::new("/work/subdir"), Some(&memory));
        let text = &member.developer_instructions;
        assert!(text.contains("relative to the workspace root `/projects/alpha`"));
        assert!(!text.contains("`/work/subdir`"));
        // Runtime roots still follow the launch cwd (plus the memory dir).
        assert_eq!(
            member.runtime_workspace_roots,
            Some(vec![PathBuf::from("/work/subdir"), scope().dir])
        );
    }

    fn member() -> LaunchAugmentation {
        LaunchAugmentation {
            harness: None,
            memory: Some(LaunchMemoryContext {
                scope: scope(),
                instructions: render_memory_instructions(&scope(), None),
                project_workspace_root: None,
            }),
        }
    }

    #[test]
    fn resolver_appends_to_configured_developer_instructions() {
        let config = serde_json::json!({ "developer_instructions": "Profile says: be terse." });
        let resolved = resolve_launch_augmentation(
            &member(),
            None,
            Some(Path::new("/work")),
            None,
            Some(&config),
        );
        let text = resolved.developer_instructions.unwrap();
        assert!(text.starts_with("Profile says: be terse.\n\n# File references\n"));
        assert!(text.contains("\n\n# Memory\n"));
        assert!(!resolved.instructions_skipped);

        // Explicit param (e.g. Release role package) is what codex would use.
        let resolved = resolve_launch_augmentation(
            &member(),
            Some("verified role".to_string()),
            Some(Path::new("/work")),
            None,
            None,
        );
        let text = resolved.developer_instructions.unwrap();
        assert!(text.starts_with("verified role\n\n# File references"));
        assert_eq!(resolved.runtime_workspace_roots, None, "roots unreadable");

        // No config instructions: sections only, independent Agent has no memory.
        let resolved = resolve_launch_augmentation(
            &LaunchAugmentation::default(),
            None,
            Some(Path::new("/work")),
            None,
            Some(&serde_json::json!({ "developer_instructions": null })),
        );
        let text = resolved.developer_instructions.unwrap();
        assert!(text.starts_with("# File references") && !text.contains("# Memory"));
        assert_eq!(resolved.runtime_workspace_roots, None);
    }

    #[test]
    fn resolver_puts_the_harness_first_and_omits_it_when_disabled() {
        let mut augmentation = member();
        augmentation.harness = Some("# Cutex\n\nstatic\n".to_string());
        let config = serde_json::json!({ "developer_instructions": "Profile says: be terse." });
        let text = resolve_launch_augmentation(&augmentation, None, Some(Path::new("/work")), None, Some(&config))
            .developer_instructions
            .unwrap();
        assert!(text.starts_with("# Cutex\n\nstatic\n\nProfile says: be terse.\n\n# File references\n"), "{text}");
        let cutex = text.find("# Cutex").unwrap();
        let cite = text.find("# File references").unwrap();
        let mem = text.find("# Memory").unwrap();
        assert!(cutex < cite && cite < mem);
        // Toggle off: launch_harness returns None and nothing is added.
        augmentation.harness = None;
        let text = resolve_launch_augmentation(&augmentation, None, Some(Path::new("/work")), None, Some(&config))
            .developer_instructions
            .unwrap();
        assert!(text.starts_with("Profile says: be terse.\n\n# File references\n") && !text.contains("# Cutex"));
    }

    #[test]
    fn resolver_skips_when_effective_instructions_are_unknown() {
        let resolved =
            resolve_launch_augmentation(&member(), None, Some(Path::new("/work")), None, None);
        assert_eq!(
            resolved,
            ResolvedLaunchAugmentation {
                developer_instructions: None,
                runtime_workspace_roots: None,
                instructions_skipped: true,
            }
        );
        let no_cwd = resolve_launch_augmentation(
            &member(),
            Some("keep".to_string()),
            None,
            None,
            Some(&serde_json::json!({})),
        );
        assert_eq!(no_cwd.developer_instructions.as_deref(), Some("keep"));
        assert!(no_cwd.instructions_skipped);
    }

    #[test]
    fn resolver_adds_memory_dir_to_configured_writable_roots() {
        let extra = if cfg!(windows) { "C:\\extra" } else { "/extra" };
        let work = if cfg!(windows) { "C:\\work" } else { "/work" };
        let legacy = serde_json::json!({
            "sandbox_workspace_write": { "writable_roots": [extra, work] }
        });
        let resolved = resolve_launch_augmentation(
            &member(),
            None,
            Some(Path::new(work)),
            None,
            Some(&legacy),
        );
        assert_eq!(
            resolved.runtime_workspace_roots,
            Some(vec![PathBuf::from(work), PathBuf::from(extra), scope().dir])
        );

        // A selected permission profile does not seed legacy roots.
        for (permissions, config) in [
            (Some(":workspace"), legacy.clone()),
            (
                None,
                serde_json::json!({
                    "default_permissions": "dev",
                    "sandbox_workspace_write": { "writable_roots": [extra] }
                }),
            ),
        ] {
            let resolved = resolve_launch_augmentation(
                &member(),
                None,
                Some(Path::new(work)),
                permissions,
                Some(&config),
            );
            assert_eq!(
                resolved.runtime_workspace_roots,
                Some(vec![PathBuf::from(work), scope().dir])
            );
        }

        // Unreadable configured roots: do not send the field.
        let relative = serde_json::json!({
            "sandbox_workspace_write": { "writable_roots": ["relative/dir"] }
        });
        let resolved = resolve_launch_augmentation(
            &member(),
            None,
            Some(Path::new(work)),
            None,
            Some(&relative),
        );
        assert_eq!(resolved.runtime_workspace_roots, None);
        assert!(resolved.developer_instructions.is_some());
    }

    #[test]
    fn merges_with_existing_instructions_and_builds_roots() {
        let context = LaunchMemoryContext {
            scope: scope(),
            instructions: "# Memory\nbody\n".to_string(),
            project_workspace_root: None,
        };
        assert_eq!(
            context.merge_developer_instructions(None),
            "# Memory\nbody\n"
        );
        assert_eq!(
            context.merge_developer_instructions(Some("role\n".to_string())),
            "role\n\n# Memory\nbody\n"
        );
        let roots = context.runtime_workspace_roots(Path::new("/work"));
        assert_eq!(roots, vec![PathBuf::from("/work"), scope().dir]);
    }
}
