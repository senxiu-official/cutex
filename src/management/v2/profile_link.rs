//! Owner-only account linking. Secrets never appear in responses or events.
use crate::http::server::{write_json_response, SimpleHttpRequest};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{net::TcpStream, path::Path};

pub const SECRET_PATH: &str = "/v2/profile-link/secret";

/// Profile reads remain available to narrowed owner devices, without links.
pub(super) fn owner_full() -> bool {
    crate::management::users::current_principal().is_none_or(|p| p.is_owner())
        && crate::management::listeners::current_client_token()
            .is_none_or(|t| t.principal.is_owner() && t.scope.is_full())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutSecret {
    secret: String,
}

fn status(home: &Path, method: &str, body: &[u8]) -> Result<Value, (u16, &'static str)> {
    use crate::profiles::account_link::{self, PutPolicy};
    let unavailable = |_| (503, "profile link secret storage is unavailable");
    let mut value = match method {
        "GET" => account_link::secret_status(home).map_err(unavailable)?,
        "PUT" => {
            let put: PutSecret = serde_json::from_slice(body)
                .map_err(|_| (400, "strict JSON object with secret required"))?;
            account_link::validate_secret(&put.secret).map_err(|_| {
                (
                    400,
                    "secret must be 32 random bytes encoded as canonical unpadded base64url",
                )
            })?;
            let policy = if crate::management::users::request_is_owner_local() {
                PutPolicy::Replace
            } else {
                PutPolicy::InitializeOnly
            };
            let change = account_link::put_secret_with_policy(home, &put.secret, policy).map_err(
                |error| {
                    if error
                        .downcast_ref::<account_link::ReplacementDenied>()
                        .is_some()
                    {
                        (
                            403,
                            "replacing a profile link secret requires owner-local access",
                        )
                    } else {
                        unavailable(error)
                    }
                },
            )?;
            audit_change(home, &change);
            change.status()
        }
        "DELETE" => {
            if !crate::management::users::request_is_owner_local() {
                return Err((
                    403,
                    "deleting a profile link secret requires owner-local access",
                ));
            }
            let change = account_link::delete_secret_change(home).map_err(unavailable)?;
            audit_change(home, &change);
            change.status()
        }
        _ => return Err((405, "use GET, PUT or DELETE for the profile link secret")),
    };
    value["contractVersion"] = json!(2);
    Ok(value)
}

fn audit_change(home: &Path, change: &crate::profiles::account_link::SecretChange) {
    if change.changed {
        crate::management::users::audit(
            &home.join("runtime/profile-link"),
            "profileLinkSecretSet",
            json!({
                "actor":crate::management::users::current_actor(),"secretIdBefore":change.secret_id_before,"secretIdAfter":change.secret_id_after,"disabled":change.disabled
            }),
        );
    }
}

pub(super) fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    if !owner_full()
        || (crate::management::listeners::current_client_token().is_none()
            && !crate::management::listeners::current_request_is_loopback())
    {
        return super::server::write_v2_error(
            stream,
            403,
            "Forbidden",
            "profile_link_owner_required",
            "profile account links require an owner device with full access",
            false,
            json!({}),
        );
    }
    let home = crate::config::paths::config_dir()?;
    match status(&home, &request.method, &request.body) {
        Ok(value) => write_json_response(stream, 200, "OK", &value),
        Err((code, message)) => super::server::write_v2_error(
            stream,
            code,
            match code {
                400 => "Bad Request",
                403 => "Forbidden",
                405 => "Method Not Allowed",
                _ => "Service Unavailable",
            },
            match code {
                400 => "invalid_request",
                403 => "owner_local_only",
                405 => "method_not_allowed",
                _ => "profile_link_unavailable",
            },
            message,
            code == 503,
            json!({}),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::management::{
        client_tokens::{DeviceScope, VerifiedClientToken},
        listeners, users,
    };
    use base64::Engine;
    struct Context;
    impl Drop for Context {
        fn drop(&mut self) {
            listeners::set_current_client_token(None);
            users::set_current_principal(None);
        }
    }
    #[test]
    fn links_require_owner_and_full_scope_even_on_reads() {
        let _reset = Context;
        assert!(owner_full());
        let mut token = VerifiedClientToken::test_token("synthetic-owner");
        for scope in [DeviceScope::Read, DeviceScope::Chat] {
            token.scope = scope;
            listeners::set_current_client_token(Some(token.clone()));
            assert!(!owner_full());
            assert_eq!(
                super::super::access::scope_need("GET", SECRET_PATH, &Value::Null, None, |_, _| {
                    None
                }),
                super::super::access::ScopeNeed::Full
            );
        }
        token.scope = DeviceScope::Full;
        listeners::set_current_client_token(Some(token));
        assert!(owner_full());
        users::set_current_principal(Some(users::Principal::Pending));
        assert!(!owner_full());
        assert_eq!(
            super::super::access::classify("PUT", SECRET_PATH),
            Some(super::super::access::Class::OwnerOnly)
        );
    }
    #[test]
    fn secret_wire_never_returns_secret_and_rejects_noncanonical_input() {
        use sha2::Digest;
        let home = std::env::temp_dir().join(format!("profile-link-http-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        assert_eq!(
            status(&home, "GET", b"").unwrap(),
            json!({"contractVersion":2,"secretId":null})
        );
        let secret = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(sha2::Sha256::digest(b"synthetic-profile-link-http-secret"));
        let body = serde_json::to_vec(&json!({"secret":secret})).unwrap();
        let value = status(&home, "PUT", &body).unwrap();
        assert!(value.get("secret").is_none());
        assert!(!value.to_string().contains(&secret));
        assert_eq!(value, status(&home, "GET", b"").unwrap());
        super::super::schema_export::assert_valid("profile-link", "secretStatus", &value);
        for bad in [
            json!({"secret":""}),
            json!({"secret":format!("{secret}=")}),
            json!({"secret":secret,"unexpected":true}),
            json!({"secret":12}),
            json!({"secret":base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([0u8;32])}),
        ] {
            assert_eq!(
                status(&home, "PUT", &serde_json::to_vec(&bad).unwrap())
                    .unwrap_err()
                    .0,
                400
            );
        }
        assert_eq!(
            status(&home, "DELETE", b"").unwrap(),
            json!({"contractVersion":2,"secretId":null,"disabled":true})
        );
        assert_eq!(status(&home, "GET", b"").unwrap()["secretId"], Value::Null);
        std::fs::remove_dir_all(home).unwrap();
    }
}
