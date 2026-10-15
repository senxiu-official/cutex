//! Stable cutex/cute-codex session identifier helpers.

pub fn normalize_cutex_session_id(cutex_session_id: &str) -> anyhow::Result<String> {
    normalize_non_path_identifier(cutex_session_id, "cutex session id")
}

pub fn normalize_codex_session_id(codex_session_id: &str) -> anyhow::Result<String> {
    normalize_non_path_identifier(codex_session_id, "Codex session id")
}

pub fn normalize_non_path_identifier(value: &str, label: &str) -> anyhow::Result<String> {
    let value = value.trim();
    if value.is_empty() {
        anyhow::bail!("{label} cannot be empty");
    }
    if value.contains('/') || value.contains('\\') {
        anyhow::bail!("{label} cannot contain path separators");
    }
    Ok(value.to_string())
}

pub fn default_cutex_session_id_for_codex_session(codex_session_id: &str) -> String {
    format!("cutex.{codex_session_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_normalization_rejects_empty_or_path_like_ids() {
        assert_eq!(
            normalize_cutex_session_id("  cutex.abc  ").expect("id should normalize"),
            "cutex.abc"
        );
        assert!(normalize_cutex_session_id(" ").is_err());
        assert!(normalize_cutex_session_id("cutex/bad").is_err());
        assert!(normalize_codex_session_id("..\\bad").is_err());
    }
}
