//! The skills a Claude session's adapter will load (`--managed-skills-root`,
//! see `structure::managed_skill_roots`), listed by cutex itself so a client
//! can offer them while the runtime is offline (`GET /v2/sessions/{id}/skills`,
//! capability `claude-session-skills`). Mirrors the adapter's bounded index
//! (`components/claude-runtime/src/cute_claude/skills.py`): `SKILL.md` in a
//! root or its immediate child directories, at most 256 skills of 48 KiB,
//! `name` / `description` / `short_description` front matter, never YAML
//! evaluation. Paths are the canonical ones the adapter's skill input accepts.
//! The answer has the Codex `skills/list` shape.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde_json::{json, Map, Value};

use crate::session::model::CutexSessionRecord;

const MAX_SKILL_BYTES: u64 = 48 * 1024;
const MAX_SKILLS: usize = 256;

/// Codex-shaped `skills/list` result for `record`'s managed roots.
pub fn list(record: &CutexSessionRecord) -> anyhow::Result<Value> {
    let roots = super::structure::managed_skill_roots(record)?;
    let workspace = Path::new(&record.cwd).canonicalize()?;
    let workspace = PathBuf::from(public(&workspace)?);
    let (skills, errors) = catalog(&roots, &workspace);
    Ok(json!({"data": [{"cwd": workspace, "skills": skills, "errors": errors}]}))
}

fn public(path: &Path) -> anyhow::Result<String> {
    Ok(crate::agent_management::strip_verbatim_prefix(path.to_str().context("skill path is not valid Unicode")?))
}

pub(crate) fn catalog(roots: &[PathBuf], workspace: &Path) -> (Vec<Value>, Vec<Value>) {
    let (mut skills, mut errors, mut seen) = (Vec::new(), Vec::new(), std::collections::HashSet::new());
    let error = |path: &Path, message: String| json!({"path": path, "message": truncate(&message, 256)});
    for root in roots {
        if !root.exists() {
            continue;
        }
        let mut candidates = Vec::new();
        if root.join("SKILL.md").is_file() {
            candidates.push(root.join("SKILL.md"));
        }
        match std::fs::read_dir(root) {
            Ok(entries) => {
                for (index, entry) in entries.enumerate() {
                    if index >= MAX_SKILLS * 4 {
                        errors.push(error(root, "managed skill root contains too many entries".into()));
                        break;
                    }
                    let Ok(entry) = entry else { continue };
                    let child = entry.path();
                    if child.is_dir() && child.join("SKILL.md").is_file() {
                        candidates.push(child.join("SKILL.md"));
                    }
                }
            }
            Err(e) => {
                errors.push(error(root, e.to_string()));
                continue;
            }
        }
        candidates.sort();
        for candidate in candidates {
            match read(&candidate, root, workspace) {
                Ok(metadata) => {
                    let path = metadata["path"].as_str().unwrap_or_default().to_string();
                    if !seen.insert(path) {
                        continue;
                    }
                    if skills.len() >= MAX_SKILLS {
                        errors.push(error(&candidate, "at most 256 managed skills are supported".into()));
                        continue;
                    }
                    skills.push(metadata);
                }
                Err(e) => errors.push(error(&candidate, format!("{e:#}"))),
            }
        }
    }
    (skills, errors)
}

fn read(candidate: &Path, root: &Path, workspace: &Path) -> anyhow::Result<Value> {
    let path = candidate.canonicalize()?;
    let path = PathBuf::from(public(&path)?);
    anyhow::ensure!(path.starts_with(root) && path.file_name().is_some_and(|n| n == "SKILL.md"), "skill path escapes its managed root");
    let meta = std::fs::metadata(&path)?;
    anyhow::ensure!(meta.is_file() && meta.len() <= MAX_SKILL_BYTES, "skill exceeds 48 KiB");
    let raw = std::fs::read(&path)?;
    anyhow::ensure!(raw.len() as u64 <= MAX_SKILL_BYTES, "skill grew beyond 48 KiB");
    let body = String::from_utf8(raw).context("skill is not UTF-8")?;
    let body = body.strip_prefix('\u{feff}').unwrap_or(&body).replace("\r\n", "\n");
    let fields = frontmatter(&body)?;
    let folder = path.parent().and_then(Path::file_name).and_then(|n| n.to_str()).unwrap_or("skill").to_string();
    let name = fields.get("name").filter(|v| !v.is_empty()).cloned().unwrap_or(folder);
    let description = fields.get("description").filter(|v| !v.is_empty()).cloned().unwrap_or_else(|| {
        body.lines()
            .map(str::trim)
            .find(|line| !line.is_empty() && *line != "---")
            .map(|line| line.trim_matches(|c| c == '#' || c == ' ').trim().to_string())
            .unwrap_or_else(|| name.clone())
    });
    anyhow::ensure!(!name.is_empty() && name.chars().count() <= 256, "skill.name must be 1..256 characters");
    anyhow::ensure!(description.chars().count() <= 8192, "skill.description exceeds 8192 characters");
    let mut metadata = Map::new();
    metadata.insert("name".into(), json!(name));
    metadata.insert("description".into(), json!(description));
    metadata.insert("path".into(), json!(path));
    metadata.insert("scope".into(), json!(if root.starts_with(workspace) { "repo" } else { "user" }));
    metadata.insert("enabled".into(), json!(true));
    metadata.insert("pluginId".into(), Value::Null);
    if let Some(short) = fields.get("short_description").filter(|v| !v.is_empty()) {
        metadata.insert("shortDescription".into(), json!(truncate(short, 1024)));
    }
    Ok(Value::Object(metadata))
}

fn truncate(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// `name` / `description` / `short_description` of a `---` front matter
/// block: plain, `"json"`, `'single'` and `>` / `|` block scalars.
fn frontmatter(body: &str) -> anyhow::Result<std::collections::HashMap<String, String>> {
    let mut fields = std::collections::HashMap::new();
    let Some(rest) = body.strip_prefix("---\n") else { return Ok(fields) };
    let end = rest.find("\n---").or_else(|| rest.starts_with("---").then_some(0)).context("skill frontmatter is not terminated")?;
    let lines: Vec<&str> = rest[..end].lines().collect();
    for (index, line) in lines.iter().enumerate() {
        let Some((key, value)) = line.split_once(':') else { continue };
        if !matches!(key, "name" | "description" | "short_description") {
            continue;
        }
        let value = value.trim_start();
        let value = if matches!(value, ">" | "|" | ">-" | "|-") {
            let continuation: Vec<&str> = lines[index + 1..]
                .iter()
                .take_while(|following| following.is_empty() || following.starts_with(char::is_whitespace))
                .map(|following| following.trim())
                .collect();
            continuation.join(if value.starts_with('|') { "\n" } else { " " }).trim().to_string()
        } else if value.starts_with('"') {
            serde_json::from_str::<String>(value).context("invalid quoted skill frontmatter")?
        } else if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
            value[1..value.len() - 1].replace("''", "'")
        } else {
            value.to_string()
        };
        fields.insert(key.to_string(), value);
    }
    Ok(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_forms() {
        let body = "---\nname: \"quoted\"\ndescription: >\n  folded\n  text\nshort_description: 'it''s'\nother: x\n---\n# Body";
        let fields = frontmatter(body).unwrap();
        assert_eq!(fields["name"], "quoted");
        assert_eq!(fields["description"], "folded text");
        assert_eq!(fields["short_description"], "it's");
        assert!(!fields.contains_key("other"));
        let literal = frontmatter("---\ndescription: |\n  a\n  b\nname: n\n---\n").unwrap();
        assert_eq!(literal["description"], "a\nb");
        assert!(frontmatter("# no front matter").unwrap().is_empty());
        assert!(frontmatter("---\nname: x\n").is_err());
    }

    #[test]
    fn catalog_lists_root_and_child_skills_with_scope() {
        let root = std::env::temp_dir().join(format!("claude-skill-catalog-{}", uuid::Uuid::new_v4()));
        let workspace = root.join("workspace");
        let repo = workspace.join(".agents/skills");
        let user = root.join("user");
        std::fs::create_dir_all(repo.join("review")).unwrap();
        std::fs::create_dir_all(repo.join("empty")).unwrap();
        std::fs::create_dir_all(user.join("notes")).unwrap();
        std::fs::create_dir_all(user.join("big")).unwrap();
        std::fs::write(repo.join("review/SKILL.md"), "---\nname: review\ndescription: Review code\nshort_description: Review\n---\nbody").unwrap();
        std::fs::write(user.join("notes/SKILL.md"), "# Take notes\n\nbody").unwrap();
        std::fs::write(user.join("big/SKILL.md"), "x".repeat(49 * 1024)).unwrap();
        let canon = |p: &Path| PathBuf::from(public(&p.canonicalize().unwrap()).unwrap());
        let (skills, errors) = catalog(&[canon(&repo), canon(&user), root.join("missing")], &canon(&workspace));
        assert_eq!(skills.len(), 2, "{skills:?}");
        assert_eq!(skills[0]["name"], "review");
        assert_eq!(skills[0]["scope"], "repo");
        assert_eq!(skills[0]["shortDescription"], "Review");
        assert_eq!(skills[0]["path"], json!(canon(&repo.join("review/SKILL.md"))));
        assert_eq!(skills[1]["name"], "notes");
        assert_eq!(skills[1]["description"], "Take notes");
        assert_eq!(skills[1]["scope"], "user");
        assert_eq!(errors.len(), 1);
        assert!(errors[0]["message"].as_str().unwrap().contains("48 KiB"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
