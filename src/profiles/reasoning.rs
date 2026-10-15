//! One reasoning-effort vocabulary for agent creation (research §8.2).
//!
//! The allowed values come, in order, from the model catalog's
//! `supported_reasoning_levels`, then the Claude effort list, then the full
//! native list; `default` (or an empty value) is always allowed and means the
//! profile / model default. There is no mapping between vocabularies: an
//! unsupported value is rejected with the allowed list.

/// Every effort the native runtime knows (`ReasoningEffort`).
pub const NATIVE_EFFORTS: &[&str] = &[
    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra", "persistent",
];

/// The value that selects the profile / model default.
pub const DEFAULT: &str = "default";

/// Efforts the catalog lists for `model`; `None` when the catalog has no
/// such model or an empty effort list (the API-key launch then accepts any
/// native effort, `selected_profile::validate_model`).
pub fn catalog_efforts(catalog: &serde_json::Value, model: &str) -> Option<Vec<String>> {
    let row = catalog["models"]
        .as_array()?
        .iter()
        .find(|row| row["slug"].as_str() == Some(model))?;
    let efforts: Vec<String> = row["supported_reasoning_levels"]
        .as_array()?
        .iter()
        .filter_map(|level| level["effort"].as_str().or_else(|| level.as_str()))
        .map(str::to_string)
        .collect();
    (!efforts.is_empty()).then_some(efforts)
}

/// The allowed values (always starting with `default`).
pub fn allowed(catalog: Option<Vec<String>>, claude: bool) -> Vec<String> {
    let listed = match catalog {
        Some(efforts) => efforts,
        None if claude => super::native_models::CLAUDE_EFFORTS.iter().map(|e| e.to_string()).collect(),
        None => NATIVE_EFFORTS.iter().map(|e| e.to_string()).collect(),
    };
    std::iter::once(DEFAULT.to_string())
        .chain(listed.into_iter().filter(|effort| effort != DEFAULT))
        .collect()
}

/// Whether `value` selects the default (`default` or empty).
pub fn is_default(value: &str) -> bool {
    let value = value.trim();
    value.is_empty() || value == DEFAULT
}

/// `Ok` when `value` is allowed; otherwise the `invalid_reasoning` detail
/// with the allowed list.
pub fn check(value: &str, allowed: &[String], context: &str) -> Result<(), String> {
    if is_default(value) || allowed.iter().any(|effort| effort == value) {
        return Ok(());
    }
    Err(format!(
        "reasoning {value:?} is not supported {context}; allowed: {}; omit it or use \"default\" for the profile default",
        serde_json::to_string(allowed).unwrap_or_default()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn catalog_then_claude_then_native() {
        let catalog = json!({"models":[{"slug":"glm","supported_reasoning_levels":[{"effort":"high"},{"effort":"max"}]},
            {"slug":"plain","supported_reasoning_levels":[]}]});
        assert_eq!(allowed(catalog_efforts(&catalog, "glm"), false), ["default", "high", "max"]);
        assert_eq!(catalog_efforts(&catalog, "plain"), None);
        assert_eq!(catalog_efforts(&catalog, "other"), None);
        assert_eq!(allowed(None, true), ["default", "low", "medium", "high", "xhigh", "max"]);
        assert_eq!(allowed(None, false).len(), NATIVE_EFFORTS.len() + 1);
    }

    #[test]
    fn check_rejects_with_the_allowed_list_and_never_maps() {
        let claude = allowed(None, true);
        assert!(check("max", &claude, "").is_ok());
        assert!(check("default", &claude, "").is_ok());
        assert!(check("", &claude, "").is_ok());
        let error = check("minimal", &claude, "for model opus").unwrap_err();
        assert!(error.contains("\"minimal\" is not supported for model opus"), "{error}");
        assert!(error.contains(r#"allowed: ["default","low","medium","high","xhigh","max"]"#), "{error}");
        assert!(check("MAX", &claude, "").is_err());
    }
}
