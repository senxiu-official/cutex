//! Read the two supported auth.json spellings consistently. This does not
//! change provider environment variable names, which remain case-sensitive.
use serde_json::Value;

pub fn from_auth(auth: &Value) -> Option<&str> {
    ["OPENAI_API_KEY", "openai_api_key"]
        .into_iter()
        .find_map(|name| {
            auth.get(name)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|key| !key.is_empty())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn supported_spellings_and_empty_canonical_fallback() {
        let key = format!("fixture-{}", uuid::Uuid::new_v4());
        for auth in [
            json!({"OPENAI_API_KEY": key}),
            json!({"openai_api_key": key}),
            json!({"OPENAI_API_KEY": null, "openai_api_key": key}),
            json!({"OPENAI_API_KEY": " ", "openai_api_key": key}),
        ] {
            assert!(
                from_auth(&auth) == Some(key.as_str()),
                "supported credential spelling not recognized"
            );
        }
        let other = format!("fixture-{}", uuid::Uuid::new_v4());
        assert!(
            from_auth(&json!({"OPENAI_API_KEY": key, "openai_api_key": other}))
                == Some(key.as_str()),
            "canonical credential must win"
        );
        assert!(from_auth(&json!({"OPENAI_API_KEY": null, "openai_api_key": " "})).is_none());
    }
}
