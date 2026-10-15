//! Query-string helpers for the simple HTTP handlers used by cutex services.

pub fn query_value(path: &str, key: &str) -> Option<String> {
    let query = path.split_once('?')?.1;
    query.split('&').find_map(|part| {
        let (name, value) = part.split_once('=')?;
        (name == key).then(|| value.to_string())
    })
}

pub fn query_has_key(path: &str, key: &str) -> bool {
    path.split_once('?')
        .map(|(_, query)| {
            query.split('&').any(|part| {
                part.split_once('=')
                    .map(|(name, _)| name == key)
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

pub fn query_bool(path: &str, key: &str) -> bool {
    query_value(path, key)
        .as_deref()
        .is_some_and(|value| matches!(value, "1" | "true" | "yes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_has_key_distinguishes_explicit_false_from_missing() {
        assert!(query_has_key(
            "/api/agents?agent_id=a&all_hosts=false",
            "all_hosts"
        ));
        assert!(query_has_key(
            "/api/agents?agent_id=a&allHosts=0",
            "allHosts"
        ));
        assert!(!query_has_key("/api/agents?agent_id=a", "all_hosts"));
        assert!(!query_bool("/api/agents?all_hosts=false", "all_hosts"));
    }
}
