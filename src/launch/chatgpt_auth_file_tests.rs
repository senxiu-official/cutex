use super::account_identity;
use serde_json::{Value, json};

pub(crate) fn auth() -> Value {
    use base64::Engine;
    let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        br#"{"https://api.openai.com/auth":{"chatgpt_user_id":"fixture-user"}}"#,
    );
    json!({"auth_mode":"chatgpt","OPENAI_API_KEY":null,"last_refresh":null,
        "tokens":{"id_token":format!("header.{claims}.signature"),
            "access_token":"synthetic-access","refresh_token":"synthetic-refresh",
            "account_id":"fixture-account",
            "oauth_provenance":{"issuer":"https://auth.example.invalid","client_id":"fixture-client"},
            "login_owner_id":"fixture-login"}})
}

fn identity(value: &Value) -> anyhow::Result<crate::role_revision::Sha256> {
    account_identity(&serde_json::to_vec(value).unwrap())
}

#[test]
fn native_login_metadata_accepts_legacy_null_and_current_native_shapes() {
    let current = auth();
    let expected = identity(&current).unwrap();
    for field in ["oauth_provenance", "login_owner_id"] {
        let mut legacy = current.clone();
        legacy["tokens"].as_object_mut().unwrap().remove(field);
        assert_eq!(identity(&legacy).unwrap(), expected);
        legacy["tokens"][field] = Value::Null;
        assert_eq!(identity(&legacy).unwrap(), expected);
    }
    let mut refreshed = current.clone();
    refreshed["tokens"]["access_token"] = json!("rotated-access");
    refreshed["tokens"]["refresh_token"] = json!("rotated-refresh");
    refreshed["tokens"]["login_owner_id"] = json!("another-login-same-account");
    refreshed["tokens"]["oauth_provenance"]["client_id"] = json!("another-client");
    assert_eq!(identity(&refreshed).unwrap(), expected);
    refreshed["tokens"]["account_id"] = json!("another-account");
    assert_ne!(identity(&refreshed).unwrap(), expected);
}

#[test]
fn native_login_metadata_rejects_malformed_types_without_echoing_secrets() {
    for (field, invalid) in [
        ("oauth_provenance", json!("synthetic-secret")),
        ("oauth_provenance", json!({"issuer":"synthetic-secret"})),
        ("oauth_provenance", json!({"issuer":false,"client_id":"synthetic-secret"})),
        ("oauth_provenance", json!({"issuer":"synthetic-secret","client_id":[]})),
        ("login_owner_id", json!({"secret":"synthetic-secret"})),
        ("login_owner_id", json!(12)),
    ] {
        let mut value = auth();
        value["tokens"][field] = invalid;
        let error = identity(&value).unwrap_err().to_string();
        assert!(error.contains("metadata"), "{error}");
        assert!(!error.contains("synthetic-secret"));
    }
}

#[test]
fn native_login_metadata_keeps_unknown_field_and_capability_rejections() {
    for (at_root, field) in [(false,"future_token_capability"), (true,"agent_identity"),
        (true,"personal_access_token"), (true,"bedrock_api_key"), (true,"bedrock_access_keys")] {
        let mut value = auth();
        if at_root { value[field] = json!("synthetic-secret"); }
        else { value["tokens"][field] = json!("synthetic-secret"); }
        assert!(identity(&value).is_err());
    }
    let mut value = auth();
    value["tokens"]["id_token"] = json!("malformed");
    assert!(identity(&value).is_err());
    value = auth();
    value["tokens"]["refresh_token"] = json!("");
    assert!(identity(&value).is_err());
}
