//! r266 Kimi accounts over Management v2 (capability `kimi-accounts`; root
//! credential incl. client / device tokens). See
//! [`crate::profiles::kimi_accounts`] and docs/development/r266-kimi-profiles.md §3.
//!
//! ```text
//! GET    /v2/kimi/accounts                         → {contractVersion, accounts: Item[]}
//! POST   /v2/kimi/accounts        {label, region?} → 201 Item
//! GET    /v2/kimi/accounts/{id}                    → Item
//! PATCH  /v2/kimi/accounts/{id}   {label?}         → Item
//! DELETE /v2/kimi/accounts/{id}[?deleteHome=true]  → {id, removed, homeDeleted, keptHome}
//! POST   /v2/kimi/accounts/{id}/login   {}         → 202 Login (new) | 200 Login (pending one)
//! GET    /v2/kimi/accounts/{id}/login/{loginId}    → Login
//! DELETE /v2/kimi/accounts/{id}/login/{loginId}    → Login (cancelled)
//! POST   /v2/kimi/accounts/{id}/logout  {}         → Item
//! POST   /v2/kimi/accounts/{id}/status  {}         → Item
//! ```
//!
//! No token is ever part of a request or a response.

use std::net::TcpStream;

use serde_json::{json, Map, Value};

use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::profiles::kimi_accounts::{self, KimiRegion, RemoveError, StartLoginError};

pub const PATH: &str = "/v2/kimi/accounts";

pub fn is_path(path: &str) -> bool {
    let path = path.split('?').next().unwrap_or(path);
    path == PATH || path.starts_with("/v2/kimi/accounts/")
}

#[derive(Debug, PartialEq, Eq)]
enum Route<'a> {
    Collection,
    Item(&'a str),
    Login(&'a str),
    LoginItem(&'a str, &'a str),
    Logout(&'a str),
    Status(&'a str),
}

fn route(path: &str) -> Option<Route<'_>> {
    let path = path.split('?').next().unwrap_or(path);
    if path == PATH {
        return Some(Route::Collection);
    }
    let rest = path.strip_prefix("/v2/kimi/accounts/")?;
    let mut parts = rest.split('/');
    let id = parts.next()?;
    kimi_accounts::validate_id(id).ok()?;
    let route = match (parts.next(), parts.next(), parts.next()) {
        (None, _, _) => Route::Item(id),
        (Some("login"), None, _) => Route::Login(id),
        (Some("login"), Some(login), None) if kimi_accounts::validate_login_id(login) => Route::LoginItem(id, login),
        (Some("logout"), None, _) => Route::Logout(id),
        (Some("status"), None, _) => Route::Status(id),
        _ => return None,
    };
    Some(route)
}

fn delete_home_requested(path: &str) -> Result<bool, String> {
    let Some((_, query)) = path.split_once('?') else { return Ok(false) };
    let mut delete_home = false;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        match pair.split_once('=').unwrap_or((pair, "")) {
            ("deleteHome", "true" | "1") => delete_home = true,
            ("deleteHome", "false" | "0") => delete_home = false,
            ("deleteHome", _) => return Err("deleteHome must be true or false".into()),
            (key, _) => return Err(format!("unknown query parameter {key}")),
        }
    }
    Ok(delete_home)
}

fn error(stream: &mut TcpStream, status: u16, code: &str, message: &str, details: Value) -> anyhow::Result<()> {
    let reason = match status {
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        _ => "Internal Server Error",
    };
    super::server::write_v2_error(stream, status, reason, code, message, status >= 500, details)
}

fn bad_request(stream: &mut TcpStream, message: &str) -> anyhow::Result<()> {
    error(stream, 400, "invalid_request", message, json!({}))
}

fn not_found(stream: &mut TcpStream, id: &str) -> anyhow::Result<()> {
    error(stream, 404, "kimi_account_not_found", &format!("no Kimi account {id}"), json!({ "id": id }))
}

fn failed(stream: &mut TcpStream, err: &anyhow::Error) -> anyhow::Result<()> {
    error(stream, 500, "kimi_accounts_failed", &format!("{err:#}"), json!({}))
}

fn write_item(stream: &mut TcpStream, status: u16, id: &str) -> anyhow::Result<()> {
    let registry = match kimi_accounts::load() {
        Ok(registry) => registry,
        Err(err) => return failed(stream, &err),
    };
    let usage = kimi_accounts::usage_by_account(&registry);
    match kimi_accounts::item_json(&registry, id, usage.get(id)) {
        Some(value) => write_json_response(stream, status, if status == 201 { "Created" } else { "OK" }, &value),
        None => not_found(stream, id),
    }
}

fn body_object(request: &SimpleHttpRequest, allowed: &[&str]) -> Result<Map<String, Value>, String> {
    if request.body.iter().all(u8::is_ascii_whitespace) {
        return Ok(Map::new());
    }
    let value: Value = serde_json::from_slice(&request.body).map_err(|_| "body must be a JSON object".to_string())?;
    let Value::Object(object) = value else { return Err("body must be a JSON object".into()) };
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(format!("unknown field {key} (allowed: {})", if allowed.is_empty() { "none".to_string() } else { allowed.join(", ") }));
    }
    Ok(object)
}

fn string_field(object: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match object.get(key) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(format!("{key}: must be a string")),
    }
}

/// `POST /v2/kimi/accounts` → `(label, region)`.
fn parse_create(request: &SimpleHttpRequest) -> Result<(String, KimiRegion), String> {
    let object = body_object(request, &["label", "region"])?;
    let label = string_field(&object, "label")?.ok_or("label is required")?;
    let label = kimi_accounts::validate_label(&label).map_err(|e| format!("label: {e}"))?;
    let region = match string_field(&object, "region")? {
        None => KimiRegion::default(),
        Some(text) => KimiRegion::parse(&text).ok_or("region: must be mainland-cn or global")?,
    };
    Ok((label, region))
}

fn label_taken(registry: &kimi_accounts::KimiAccountsFile, label: &str, except: Option<&str>) -> bool {
    registry.accounts.iter().any(|account| Some(account.id.as_str()) != except && account.label == label)
}

fn login_response(stream: &mut TcpStream, status: u16, login: &kimi_accounts::LoginSnapshot) -> anyhow::Result<()> {
    let reason = if status == 202 { "Accepted" } else { "OK" };
    let mut body = login.to_wire();
    body["contractVersion"] = json!(2);
    write_json_response(stream, status, reason, &body)
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let Some(route) = route(&request.path) else {
        return error(stream, 404, "not_found", "unknown Kimi accounts route", json!({}));
    };
    let registry = match kimi_accounts::load() {
        Ok(registry) => registry,
        Err(err) => return failed(stream, &err),
    };
    let known = |id: &str| registry.find(id).is_some();
    match (request.method.as_str(), route) {
        ("GET", Route::Collection) => match kimi_accounts::list_json() {
            Ok(value) => write_json_response(stream, 200, "OK", &value),
            Err(err) => failed(stream, &err),
        },
        ("POST", Route::Collection) => {
            let (label, region) = match parse_create(request) {
                Ok(create) => create,
                Err(message) => return bad_request(stream, &message),
            };
            if label_taken(&registry, &label, None) {
                return error(stream, 409, "kimi_account_label_taken", &format!("a Kimi account labelled {label} already exists"), json!({ "label": label }));
            }
            match kimi_accounts::add(&label, region) {
                Ok(account) => write_item(stream, 201, &account.id),
                Err(err) => failed(stream, &err),
            }
        }
        ("GET", Route::Item(id)) => {
            if !known(id) {
                return not_found(stream, id);
            }
            write_item(stream, 200, id)
        }
        ("PATCH", Route::Item(id)) => {
            if !known(id) {
                return not_found(stream, id);
            }
            let object = match body_object(request, &["label"]) {
                Ok(object) => object,
                Err(message) => return bad_request(stream, &message),
            };
            let label = match string_field(&object, "label") {
                Ok(label) => label,
                Err(message) => return bad_request(stream, &message),
            };
            if let Some(label) = label {
                let label = match kimi_accounts::validate_label(&label) {
                    Ok(label) => label,
                    Err(err) => return bad_request(stream, &format!("label: {err}")),
                };
                if label_taken(&registry, &label, Some(id)) {
                    return error(stream, 409, "kimi_account_label_taken", &format!("a Kimi account labelled {label} already exists"), json!({ "label": label }));
                }
                if let Err(err) = kimi_accounts::rename(id, &label) {
                    return failed(stream, &err);
                }
            }
            write_item(stream, 200, id)
        }
        ("DELETE", Route::Item(id)) => {
            let delete_home = match delete_home_requested(&request.path) {
                Ok(delete_home) => delete_home,
                Err(message) => return bad_request(stream, &message),
            };
            match kimi_accounts::remove(id, delete_home) {
                Ok(outcome) => write_json_response(stream, 200, "OK", &json!({
                    "id": outcome.removed.id, "removed": true, "homeDeleted": outcome.home_deleted, "keptHome": outcome.kept_home,
                })),
                Err(RemoveError::NotFound) => not_found(stream, id),
                Err(err @ RemoveError::InUse(_)) => {
                    let RemoveError::InUse(usage) = &err else { unreachable!() };
                    let details = json!({
                        "id": id,
                        "profiles": usage.profiles,
                        "sessions": usage.sessions.iter().filter(|s| s.lifecycle == "active").collect::<Vec<_>>(),
                    });
                    error(stream, 409, "kimi_account_in_use", &err.to_string(), details)
                }
                Err(RemoveError::Other(err)) => failed(stream, &err),
            }
        }
        ("POST", Route::Login(id)) => {
            if let Err(message) = body_object(request, &[]) {
                return bad_request(stream, &message);
            }
            match kimi_accounts::start_login(id, kimi_accounts::LOGIN_CODE_WAIT) {
                Ok((login, created)) => login_response(stream, if created { 202 } else { 200 }, &login),
                Err(StartLoginError::NotFound) => not_found(stream, id),
                Err(StartLoginError::CliUnavailable(message)) => {
                    let (status, reason) = crate::profiles::kimi::component_status();
                    error(stream, 409, "kimi_runtime_unavailable", &message, json!({ "id": id, "status": status.as_str(), "reason": reason }))
                }
                Err(StartLoginError::Other(err)) => failed(stream, &err),
            }
        }
        ("GET", Route::LoginItem(id, login_id)) => {
            if !known(id) {
                return not_found(stream, id);
            }
            match kimi_accounts::login_by_id(id, login_id) {
                Some(login) => login_response(stream, 200, &login),
                None => error(stream, 404, "kimi_login_not_found", "no such login (finished long ago, or the daemon restarted)", json!({ "id": id, "loginId": login_id })),
            }
        }
        ("DELETE", Route::LoginItem(id, login_id)) => {
            if !known(id) {
                return not_found(stream, id);
            }
            match kimi_accounts::cancel_login(id, login_id) {
                Some(login) => login_response(stream, 200, &login),
                None => error(stream, 404, "kimi_login_not_found", "no such login", json!({ "id": id, "loginId": login_id })),
            }
        }
        ("POST", Route::Logout(id)) => {
            if !known(id) {
                return not_found(stream, id);
            }
            if let Err(message) = body_object(request, &[]) {
                return bad_request(stream, &message);
            }
            match kimi_accounts::logout(id) {
                Ok(_) => write_item(stream, 200, id),
                Err(err) => failed(stream, &err),
            }
        }
        ("POST", Route::Status(id)) => {
            if !known(id) {
                return not_found(stream, id);
            }
            if let Err(message) = body_object(request, &[]) {
                return bad_request(stream, &message);
            }
            write_item(stream, 200, id)
        }
        (method, _) => error(stream, 405, "method_not_allowed", &format!("{method} is not allowed on this Kimi accounts route"), json!({})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(body: &str) -> SimpleHttpRequest {
        SimpleHttpRequest { method: "POST".into(), path: PATH.into(), headers: Default::default(), body: body.as_bytes().to_vec() }
    }

    #[test]
    fn routes_accept_account_and_login_ids_only() {
        let login = "kl-0123456789abcdef0123456789abcdef";
        assert_eq!(route("/v2/kimi/accounts"), Some(Route::Collection));
        assert_eq!(route("/v2/kimi/accounts/ka-abc"), Some(Route::Item("ka-abc")));
        assert_eq!(route("/v2/kimi/accounts/ka-abc?deleteHome=true"), Some(Route::Item("ka-abc")));
        assert_eq!(route("/v2/kimi/accounts/ka-abc/login"), Some(Route::Login("ka-abc")));
        assert_eq!(route(&format!("/v2/kimi/accounts/ka-abc/login/{login}")), Some(Route::LoginItem("ka-abc", login)));
        assert_eq!(route("/v2/kimi/accounts/ka-abc/logout"), Some(Route::Logout("ka-abc")));
        assert_eq!(route("/v2/kimi/accounts/ka-abc/status"), Some(Route::Status("ka-abc")));
        for bad in ["/v2/kimi/accounts/default", "/v2/kimi/accounts/../x", "/v2/kimi/accounts/ka-abc/login/kl-x",
                    "/v2/kimi/accounts/ka-abc/login/KL-0123456789ABCDEF0123456789ABCDEF", "/v2/kimi/accounts/ka-abc/token",
                    "/v2/kimi/accounts/ka-abc/logout/x"] {
            assert_eq!(route(bad), None, "{bad}");
        }
        assert!(is_path("/v2/kimi/accounts?x") && !is_path("/v2/kimi/accountsx") && !is_path("/v2/claude/accounts"));
    }

    #[test]
    fn create_bodies_are_validated() {
        assert_eq!(parse_create(&request(r#"{"label":"Work"}"#)).unwrap(), ("Work".into(), KimiRegion::MainlandCn));
        assert_eq!(parse_create(&request(r#"{"label":"W","region":"global"}"#)).unwrap().1, KimiRegion::Global);
        for body in [r#"{}"#, r#"{"label":""}"#, r#"{"label":"x","region":"eu"}"#, r#"{"label":"x","token":"t"}"#, r#"[]"#, r#"{"label":1}"#] {
            assert!(parse_create(&request(body)).is_err(), "{body}");
        }
        assert!(body_object(&request(""), &[]).unwrap().is_empty());
        assert!(body_object(&request(r#"{"code":"x"}"#), &[]).is_err());
    }
}
