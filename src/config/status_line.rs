//! Profile-scoped frontend preferences. Runtime/authentication homes are unrelated.
use std::path::{Path, PathBuf};
use anyhow::Context;

pub fn load(profile_id: Option<&str>) -> anyhow::Result<(PathBuf, toml::Value)> {
    load_at(&super::paths::config_dir()?, profile_id)
}

fn load_at(root: &Path, profile_id: Option<&str>) -> anyhow::Result<(PathBuf, toml::Value)> {
    let shared = root.join("status-line.toml");
    let path = match profile_id {
        Some(id) => {
            anyhow::ensure!(!id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'), "Invalid status line profile ID");
            root.join("status-lines").join(format!("{id}.toml"))
        }
        None => shared.clone(),
    };
    let read = |p: &Path| -> anyhow::Result<Option<toml::Value>> {
        match std::fs::read_to_string(p) {
            Ok(text) => Ok(Some(toml::from_str(&text).context("Invalid saved Cutex status line preferences")?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    };
    let saved = match read(&path)? {
        Some(value) => value,
        None => read(&shared)?.unwrap_or_else(|| toml::Value::Table(Default::default())),
    };
    Ok((path, saved))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn profiles_inherit_once_and_keep_separate_saved_choices() {
        let root = std::env::temp_dir().join(format!("cutex-status-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&root.join("status-line.toml"), "[tui]\nstatus_line=['model-name']\n").unwrap();
        let (a, initial) = load_at(&root, Some("profile-a")).unwrap();
        let (b, other) = load_at(&root, Some("profile-b")).unwrap();
        assert_ne!(a,b); assert_eq!(initial,other);
        std::fs::create_dir_all(a.parent().unwrap()).unwrap();
        std::fs::write(&a, "[tui]\nstatus_line=[]\nstatus_line_use_colors=false\n").unwrap();
        assert_eq!(load_at(&root, Some("profile-a")).unwrap().1["tui"]["status_line"].as_array().unwrap().len(),0);
        assert_eq!(load_at(&root, Some("profile-b")).unwrap().1,initial);
        assert!(load_at(&root,Some("../escape")).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
