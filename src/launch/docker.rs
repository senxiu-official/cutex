//! Historical Docker record labels; executable Docker integration has been removed.

pub fn docker_user_name(input: Option<&str>) -> anyhow::Result<String> {
    match input {
        Some(value) => normalize_docker_user_name(Some(value.to_string())),
        None => Ok(default_docker_user_name()),
    }
}

pub fn default_docker_user_name() -> String {
    std::env::var("USER")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| {
            !value.is_empty()
                && value != "."
                && value != ".."
                && !value.starts_with('-')
                && !value.contains('/')
                && !value.contains('\\')
                && value
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
        })
        .unwrap_or_else(|| "cutex".to_string())
}

pub fn normalize_docker_user_name(input: Option<String>) -> anyhow::Result<String> {
    let value = input
        .unwrap_or_else(default_docker_user_name)
        .trim()
        .to_string();

    if value.is_empty() {
        anyhow::bail!("Docker user name cannot be empty");
    }

    if value == "." || value == ".." {
        anyhow::bail!("Docker user name cannot be '.' or '..'");
    }

    if value.contains('/') || value.contains('\\') {
        anyhow::bail!("Docker user name cannot contain path separators");
    }

    if value.starts_with('-') {
        anyhow::bail!("Docker user name cannot start with '-'");
    }

    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
    {
        anyhow::bail!("Docker user name may only contain ASCII letters, digits, '.', '_' or '-'");
    }

    Ok(value)
}
