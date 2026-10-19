//! Management client tokens (root credential or a client token):
//!
//! - `GET    /v2/client-tokens` → `{hostId, tokens: [{tokenId, label, kind,
//!   deviceName, platform, ownerName, createdAt, lastUsedAt, lastAddress}]}`
//!   (never secrets or hashes)
//! - `PATCH  /v2/client-tokens/{tokenId}` `{name: string|null}` → `{tokenId,
//!   ownerName}` (capability `client-token-rename`): sets (trimmed, 1..=60
//!   characters, no control characters) or clears (`null`, or a blank
//!   string) the owner-given display name. The label (which clients use to
//!   find their own token) never changes. 400 `invalid_client_token_name`,
//!   404 `client_token_not_found`
//!   With `scope: "full"|"chat"|"read"` (capability `device-scope`; either
//!   field or both) it sets the token's device scope → `{tokenId, ownerName,
//!   scope}` (400 `invalid_scope`); a narrowed device's open streams end
//!   (`cutex/access/changed`). Audit `tokenScopeChanged {tokenId, from, to,
//!   actor}` in the users audit log.
//! - `DELETE /v2/client-tokens/{tokenId}` → `{tokenId, revoked: true}`;
//!   404 `client_token_not_found`
//!
//! The access gate keeps every route here to the owner (root, or a token
//! bound to the owner).
//!
//! Tokens are issued only on the host itself (`cutex management client-token
//! issue --label …`, e.g. over ssh), never over HTTP: the secret is printed
//! once to the issuing terminal. See `crate::management::client_tokens`.

use std::net::TcpStream;

use serde_json::json;

use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::management::client_tokens::{validate_owner_name, ClientTokenStore, DeviceScope, TOKEN_ID_PREFIX};

pub const PATH: &str = "/v2/client-tokens";

pub fn is_client_tokens_path(path: &str) -> bool {
    path == PATH || path.starts_with("/v2/client-tokens/")
}

fn token_id_from_path(path: &str) -> Option<&str> {
    let id = path.strip_prefix("/v2/client-tokens/")?;
    (id.starts_with(TOKEN_ID_PREFIX)
        && id.len() <= 64
        && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'))
    .then_some(id)
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    match ClientTokenStore::open_default() {
        Ok(store) => handle_with_store(stream, request, &store),
        Err(error) => super::server::write_v2_error(
            stream,
            503,
            "Service Unavailable",
            "client_tokens_unavailable",
            &format!("the client token store is unavailable: {error:#}"),
            true,
            json!({}),
        ),
    }
}

pub(crate) fn handle_with_store(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    store: &ClientTokenStore,
) -> anyhow::Result<()> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    match (request.method.as_str(), path == PATH, token_id_from_path(path)) {
        ("GET", true, _) => match store.list() {
            Ok(tokens) => write_json_response(
                stream,
                200,
                "OK",
                &json!({
                    "hostId": crate::platform::host::current_host_name(),
                    "tokens": tokens,
                }),
            ),
            Err(error) => unavailable(stream, &error),
        },
        ("PATCH", false, Some(token_id)) => {
            let patch = match parse_patch(&request.body) {
                Ok(patch) => patch,
                Err((code, message)) => {
                    return super::server::write_v2_error(
                        stream,
                        400,
                        "Bad Request",
                        code,
                        &message,
                        false,
                        json!({
                            "maxChars": crate::management::client_tokens::MAX_OWNER_NAME_CHARS,
                            "scopes": ["full", "chat", "read"],
                        }),
                    )
                }
            };
            match apply_patch(store, token_id, &patch) {
                Ok(Some(value)) => write_json_response(stream, 200, "OK", &value),
                Ok(None) => super::server::write_v2_error(
                    stream,
                    404,
                    "Not Found",
                    "client_token_not_found",
                    "no client token with this id",
                    false,
                    json!({ "tokenId": token_id }),
                ),
                Err(error) => unavailable(stream, &error),
            }
        }
        ("DELETE", false, Some(token_id)) => match store.revoke(token_id) {
            Ok(true) => write_json_response(
                stream,
                200,
                "OK",
                &json!({ "tokenId": token_id, "revoked": true }),
            ),
            Ok(false) => super::server::write_v2_error(
                stream,
                404,
                "Not Found",
                "client_token_not_found",
                "no client token with this id",
                false,
                json!({ "tokenId": token_id }),
            ),
            Err(error) => unavailable(stream, &error),
        },
        (_, true, _) | (_, false, Some(_)) => super::server::write_v2_error(
            stream,
            405,
            "Method Not Allowed",
            "method_not_allowed",
            "unsupported method for this route",
            false,
            json!({}),
        ),
        _ => super::server::write_v2_error(
            stream,
            404,
            "Not Found",
            "not_found",
            "unknown client token route",
            false,
            json!({}),
        ),
    }
}

#[derive(Debug, Default)]
struct Patch {
    /// `Some(None)` clears the name.
    name: Option<Option<String>>,
    scope: Option<DeviceScope>,
}

/// `{name?: string|null, scope?: "full"|"chat"|"read"}` (at least one):
/// the validated name (`None` = clear; a blank string clears too) and scope.
fn parse_patch(body: &[u8]) -> Result<Patch, (&'static str, String)> {
    let invalid_name = |message: String| ("invalid_client_token_name", message);
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| invalid_name("body must be a JSON object".into()))?;
    let object = value.as_object().ok_or_else(|| invalid_name("body must be a JSON object".into()))?;
    let mut patch = Patch::default();
    match object.get("name") {
        None => {}
        Some(serde_json::Value::Null) => patch.name = Some(None),
        Some(serde_json::Value::String(name)) if name.trim().is_empty() => patch.name = Some(None),
        Some(serde_json::Value::String(name)) => {
            patch.name = Some(Some(validate_owner_name(name).map_err(invalid_name)?))
        }
        Some(_) => return Err(invalid_name("name must be a string or null".into())),
    }
    match object.get("scope") {
        None => {}
        Some(value) => {
            let scope = value.as_str().and_then(DeviceScope::parse).ok_or((
                "invalid_scope",
                "scope must be \"full\", \"chat\" or \"read\"".to_string(),
            ))?;
            patch.scope = Some(scope);
        }
    }
    if patch.name.is_none() && patch.scope.is_none() {
        return Err(invalid_name("name is required (a string, or null to clear), or scope".into()));
    }
    Ok(patch)
}

/// Apply a PATCH → `{tokenId, ownerName, scope}`; `Ok(None)` = no such token.
fn apply_patch(store: &ClientTokenStore, token_id: &str, patch: &Patch) -> anyhow::Result<Option<serde_json::Value>> {
    if let Some(name) = &patch.name {
        if store.rename(token_id, name.as_deref())?.is_none() {
            return Ok(None);
        }
    }
    if let Some(scope) = patch.scope {
        let Some((from, to)) = store.set_scope(token_id, scope)? else {
            return Ok(None);
        };
        if from != to {
            crate::management::users::audit(
                crate::management::users::UserStore::beside(store).dir(),
                "tokenScopeChanged",
                json!({ "tokenId": token_id, "from": from, "to": to, "actor": crate::management::users::current_actor() }),
            );
        }
    }
    let Some(token) = store.list()?.into_iter().find(|token| token.token_id == token_id) else {
        return Ok(None);
    };
    Ok(Some(json!({ "tokenId": token_id, "ownerName": token.owner_name, "scope": token.scope })))
}

fn unavailable(stream: &mut TcpStream, error: &anyhow::Error) -> anyhow::Result<()> {
    super::server::write_v2_error(
        stream,
        503,
        "Service Unavailable",
        "client_tokens_unavailable",
        &format!("the client token store is unavailable: {error:#}"),
        true,
        json!({}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::management::client_tokens::authenticate_management_request_with;
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener};

    /// Send `raw` over a real socket, authenticate like the Management
    /// server (client token → root), apply the root check, then route.
    fn roundtrip(raw: String, store: &ClientTokenStore, root: &str) -> String {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let mut stream = std::net::TcpStream::connect(address).unwrap();
            stream.write_all(raw.as_bytes()).unwrap();
            stream.shutdown(Shutdown::Write).unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            response
        });
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = crate::http::server::read_simple_http_request(&mut stream).unwrap();
        authenticate_management_request_with(&mut request, Some(root), store);
        if super::super::server::authenticated_root_request(&request, Some(root)) {
            handle_with_store(&mut stream, &request, store).unwrap();
        } else {
            super::super::server::write_v2_error(
                &mut stream,
                401,
                "Unauthorized",
                "unauthorized",
                "root required",
                false,
                json!({}),
            )
            .unwrap();
        }
        drop(stream);
        client.join().unwrap()
    }

    fn get(bearer: &str) -> String {
        format!(
            "GET /v2/client-tokens HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {bearer}\r\nConnection: close\r\n\r\n"
        )
    }

    #[test]
    fn client_token_lists_and_revokes_then_is_refused() {
        let store = ClientTokenStore::at(
            std::env::temp_dir()
                .join(format!("cutex-client-token-api-{}", uuid::Uuid::new_v4()))
                .join("management-clients"),
        );
        let root = "management-root-secret";
        let issued = store.issue("desktop-test").unwrap();
        let other = store.issue("other").unwrap();

        let response = roundtrip(get(&issued.token), &store, root);
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        assert!(response.contains("desktop-test"));
        assert!(response.contains(&issued.token_id));
        assert!(!response.contains(&issued.token));
        assert!(!response.contains(root));
        assert!(!response.contains("tokenSha256"));

        // The root credential works too; an unknown client token does not.
        assert!(roundtrip(get(root), &store, root).starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(roundtrip(get("cxm1.AAAA"), &store, root).starts_with("HTTP/1.1 401 "));

        let delete = format!(
            "DELETE /v2/client-tokens/{} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
            other.token_id, issued.token
        );
        let response = roundtrip(delete.clone(), &store, root);
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        assert!(roundtrip(delete, &store, root).starts_with("HTTP/1.1 404 "));
        assert!(roundtrip(get(&other.token), &store, root).starts_with("HTTP/1.1 401 "));
        // A client token can revoke itself; afterwards it is refused.
        store.revoke(&issued.token_id).unwrap();
        assert!(roundtrip(get(&issued.token), &store, root).starts_with("HTTP/1.1 401 "));
    }

    fn patch(token_id: &str, bearer: &str, body: &str) -> String {
        format!(
            "PATCH /v2/client-tokens/{token_id} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {bearer}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn patch_renames_and_clears_and_validates() {
        let store = ClientTokenStore::at(
            std::env::temp_dir()
                .join(format!("cutex-client-token-rename-{}", uuid::Uuid::new_v4()))
                .join("management-clients"),
        );
        let root = "management-root-secret";
        let issued = store.issue("desktop-mhs-087278-Cutex-257920").unwrap();

        let response = roundtrip(patch(&issued.token_id, root, r#"{"name":"  Office PC  "}"#), &store, root);
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        assert!(response.contains(r#""ownerName":"Office PC""#), "{response}");
        let listed = roundtrip(get(root), &store, root);
        assert!(listed.contains(r#""ownerName":"Office PC""#), "{listed}");
        assert!(listed.contains("desktop-mhs-087278-Cutex-257920"));

        // A client token (root-equivalent) may rename too.
        let response = roundtrip(patch(&issued.token_id, &issued.token, r#"{"name":"Laptop"}"#), &store, root);
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");

        for bad in [
            r#"{"name":"a\u0001b"}"#.to_string(),
            format!(r#"{{"name":"{}"}}"#, "x".repeat(61)),
            r#"{"name":5}"#.to_string(),
            r#"{}"#.to_string(),
            "not json".to_string(),
        ] {
            let response = roundtrip(patch(&issued.token_id, root, &bad), &store, root);
            assert!(response.starts_with("HTTP/1.1 400 "), "{bad}: {response}");
            assert!(response.contains("invalid_client_token_name"), "{response}");
        }
        assert_eq!(store.list().unwrap()[0].owner_name.as_deref(), Some("Laptop"));

        let response = roundtrip(patch(&issued.token_id, root, r#"{"name":null}"#), &store, root);
        assert!(response.contains(r#""ownerName":null"#), "{response}");
        assert_eq!(store.list().unwrap()[0].owner_name, None);
        roundtrip(patch(&issued.token_id, root, r#"{"name":"x"}"#), &store, root);
        let response = roundtrip(patch(&issued.token_id, root, r#"{"name":"   "}"#), &store, root);
        assert!(response.contains(r#""ownerName":null"#), "{response}");

        let response = roundtrip(patch("mct-0000000000000000", root, r#"{"name":"x"}"#), &store, root);
        assert!(response.starts_with("HTTP/1.1 404 "), "{response}");
        // Without credentials: 401.
        let response = roundtrip(patch(&issued.token_id, "cxm1.AAAA", r#"{"name":"x"}"#), &store, root);
        assert!(response.starts_with("HTTP/1.1 401 "), "{response}");
    }

    #[test]
    fn token_id_paths_are_strict() {
        assert!(is_client_tokens_path("/v2/client-tokens"));
        assert_eq!(token_id_from_path("/v2/client-tokens/mct-0123abcd"), Some("mct-0123abcd"));
        assert_eq!(token_id_from_path("/v2/client-tokens/../x"), None);
        assert_eq!(token_id_from_path("/v2/client-tokens/other"), None);
    }
}
