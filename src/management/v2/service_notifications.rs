//! Management API for owner-visible notices posted by registered services.

use std::net::TcpStream;

use chrono::{Local, Utc};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use super::service_notification_receivers as receivers;
use crate::http::server::{
    extra_response_headers, set_extra_response_headers, write_json_response, SimpleHttpRequest,
};
use crate::management::listeners::current_request_is_loopback;
use crate::services::{
    notifications as notices,
    store::{validate_slug, ServiceRecord, ServiceStore, ServicesError, ServicesResult},
};

const HISTORY_PATH: &str = "/v2/service-notifications";
const MAX_BODY_BYTES: usize = 4096;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IssueCredential {
    mode: CredentialMode,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum CredentialMode {
    Manual,
    Supervised,
}

pub(super) enum Route<'a> {
    Publish(&'a str),
    Configuration(&'a str),
    Credential(&'a str),
    History,
    Notice(&'a str, &'a str),
    Receiver,
}

pub(super) fn parse_route(path: &str) -> Option<Route<'_>> {
    if path == HISTORY_PATH {
        return Some(Route::History);
    }
    if path == receivers::PATH {
        return Some(Route::Receiver);
    }
    let (slug, suffix) = path.strip_prefix("/v2/services/")?.split_once('/')?;
    if validate_slug(slug).is_err() {
        return None;
    }
    match suffix {
        "notifications" => Some(Route::Publish(slug)),
        "notification-settings" => Some(Route::Configuration(slug)),
        "notification-credential" => Some(Route::Credential(slug)),
        _ => {
            let id = suffix.strip_prefix("notifications/")?;
            (id.len() == 36 && uuid::Uuid::parse_str(id).is_ok()).then_some(Route::Notice(slug, id))
        }
    }
}

fn decode<T: DeserializeOwned>(request: &SimpleHttpRequest) -> ServicesResult<T> {
    if request.body.len() > MAX_BODY_BYTES {
        return Err(ServicesError::new(
            413,
            "service_notification_too_large",
            "service notification body is too large",
        ));
    }
    serde_json::from_slice(&request.body)
        .map_err(|_| notices::bad_request("invalid service notification JSON"))
}

fn unavailable(_: impl std::fmt::Display) -> ServicesError {
    ServicesError::new(
        503,
        "service_notifications_unavailable",
        "service notification storage is unavailable",
    )
}

fn unauthorized() -> ServicesError {
    ServicesError::new(
        401,
        "service_credential_required",
        "a credential for this service instance is required",
    )
}

fn require_publisher(request: &SimpleHttpRequest) -> ServicesResult<&str> {
    if !current_request_is_loopback()
        || crate::management::relay::current_peer().is_some()
        || request.headers.contains_key("origin")
        || request.headers.contains_key("sec-fetch-site")
    {
        return Err(unauthorized());
    }
    request
        .headers
        .get("authorization")
        .and_then(|header| header.strip_prefix("Bearer "))
        .filter(|token| {
            token.starts_with(notices::TOKEN_PREFIX)
                && token.len() == notices::TOKEN_PREFIX.len() + 43
        })
        .ok_or_else(unauthorized)
}

fn configure<T>(
    registry: &ServiceStore,
    slug: &str,
    operation: impl FnOnce(&ServiceRecord) -> ServicesResult<T>,
) -> ServicesResult<T> {
    registry.mutate(|data| {
        let service = data
            .services
            .get_mut(slug)
            .ok_or_else(|| ServicesError::not_found(slug))?;
        if service.archived {
            return Err(ServicesError::new(
                409,
                "service_archived",
                "restore the service before configuring notifications",
            ));
        }
        service
            .notification_instance
            .get_or_insert_with(|| uuid::Uuid::new_v4().to_string());
        operation(service)
    })
}

fn history(
    request: &SimpleHttpRequest,
    storage: &notices::Store,
    now: i64,
) -> ServicesResult<Value> {
    let mut slug = None;
    let mut before = None;
    let mut limit = None;
    let mut after = None;
    if let Some((_, query)) = request.path.split_once('?') {
        for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
            match name.as_ref() {
                "service" if slug.is_none() && validate_slug(&value).is_ok() => {
                    slug = Some(value.into_owned())
                }
                "before" if before.is_none() => {
                    before = Some(
                        value
                            .parse::<i64>()
                            .ok()
                            .filter(|n| *n > 0)
                            .ok_or_else(|| {
                                notices::bad_request("before must be a positive cursor")
                            })?,
                    )
                }
                "limit" if limit.is_none() => {
                    limit = Some(
                        value
                            .parse::<usize>()
                            .ok()
                            .filter(|n| (1..=100).contains(n))
                            .ok_or_else(|| {
                                notices::bad_request("limit must be between 1 and 100")
                            })?,
                    )
                }
                "after" if after.is_none() => {
                    after = Some(value.parse::<i64>().ok().filter(|n| *n >= 0).ok_or_else(
                        || notices::bad_request("after must be a nonnegative cursor"),
                    )?)
                }
                _ => return Err(notices::bad_request("invalid service notification query")),
            }
        }
    }
    let count = limit.unwrap_or(50);
    if let Some(after) = after {
        if before.is_some() {
            return Err(notices::bad_request(
                "before and after are mutually exclusive",
            ));
        }
        let notifications: Vec<_> = storage
            .feed(slug.as_deref(), after, count, now)?
            .into_iter()
            .map(|entry| {
                json!({"id": entry.notice.id, "slug": entry.notice.slug, "cursor": entry.cursor,
                "desktop": entry.notice.desktop, "phone": entry.notice.phone})
            })
            .collect();
        return Ok(json!({"streamId": storage.stream_id()?, "notifications": notifications}));
    }
    let mut items = storage.history(slug.as_deref(), before.unwrap_or(i64::MAX), count + 1, now)?;
    let next = if items.len() > count {
        items.pop();
        items.last().map(|entry| entry.cursor)
    } else {
        None
    };
    Ok(
        json!({"notifications": items, "nextBefore": next, "retention": {
            "maxAgeSeconds": notices::HISTORY_AGE,
            "maxPerService": notices::HISTORY_PER_SERVICE,
            "maxPerHost": notices::HISTORY_PER_HOST,
        }}),
    )
}

fn host_timezone_name() -> Option<String> {
    #[cfg(unix)]
    if let Some(value) = std::env::var_os("TZ") {
        let name = value.to_str()?.trim_start_matches(':');
        let name = name.strip_prefix("/usr/share/zoneinfo/").unwrap_or(name);
        if name.is_empty()
            || !name
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != "..")
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"/_+-.".contains(&byte))
        {
            return None;
        }
        // A POSIX TZ rule can override the OS timezone without naming an IANA zone.
        return std::path::Path::new("/usr/share/zoneinfo")
            .join(name)
            .is_file()
            .then(|| name.to_owned());
    }
    iana_time_zone::get_timezone().ok()
}

fn configuration_view(view: notices::Configuration) -> Value {
    json!({
        "settings": view.settings,
        "credentialConfigured": view.credential_configured,
        "supervisedCredential": view.supervised_credential,
        "timezone": {"iana": host_timezone_name(), "offsetSeconds": Local::now().offset().local_minus_utc()},
        "phone": receivers::status(),
    })
}

fn process(
    request: &SimpleHttpRequest,
    route: Route<'_>,
    owner: bool,
) -> ServicesResult<(u16, Value)> {
    if let Route::Publish(slug) = route {
        if request.method != "POST" {
            return Err(method_not_allowed());
        }
        let token = require_publisher(request)?;
        let registry = ServiceStore::open_default().map_err(unavailable)?;
        let mut storage = notices::Store::open(registry.dir())?;
        let input = decode(request)?;
        let receipt = registry.mutate(|data| {
            let service = data.services.get(slug).ok_or_else(unauthorized)?;
            storage.submit(service, token, input, Utc::now())
        })?;
        if let Some(entry) = &receipt.delivery {
            super::summary::service_notification_ready(entry);
        }
        let status = if receipt.disposition == notices::Disposition::Accepted {
            201
        } else {
            200
        };
        return Ok((status, serde_json::to_value(receipt).map_err(unavailable)?));
    }
    if !owner {
        return Err(ServicesError::new(
            401,
            "owner_required",
            "a full Management owner credential is required",
        ));
    }
    if matches!(route, Route::Receiver) {
        if request.method != "POST" {
            return Err(method_not_allowed());
        }
        receivers::report(request, decode(request)?)?;
        return Ok((200, json!({"accepted": true})));
    }
    let registry = ServiceStore::open_default().map_err(unavailable)?;
    let storage = notices::Store::open(registry.dir())?;
    match (request.method.as_str(), route) {
        ("GET", Route::History) => Ok((200, history(request, &storage, Utc::now().timestamp())?)),
        ("GET", Route::Configuration(slug)) => Ok((
            200,
            configuration_view(storage.configuration(&registry.get(slug)?)?),
        )),
        ("PUT", Route::Configuration(slug)) => {
            let settings = decode(request)?;
            let view = configure(&registry, slug, |service| {
                storage.set_settings(service, &settings)?;
                storage.configuration(service)
            })?;
            Ok((200, configuration_view(view)))
        }
        ("POST", Route::Credential(slug)) => {
            let mode = if request.body.is_empty() {
                CredentialMode::Manual
            } else {
                decode::<IssueCredential>(request)?.mode
            };
            match mode {
                CredentialMode::Manual => {
                    let token = configure(&registry, slug, |service| {
                        storage.rotate_credential(service)
                    })?;
                    Ok((201, json!({"token": token})))
                }
                CredentialMode::Supervised => {
                    configure(&registry, slug, |service| {
                        storage.assign_supervised_credential(service)
                    })?;
                    Ok((200, json!({"restartRequired": true})))
                }
            }
        }
        ("DELETE", Route::Credential(slug)) => {
            registry.mutate(|data| {
                storage.revoke(
                    data.services
                        .get(slug)
                        .ok_or_else(|| ServicesError::not_found(slug))?,
                )
            })?;
            Ok((200, json!({"revoked": true})))
        }
        ("GET", Route::Notice(slug, id)) => {
            let entry = storage
                .get(&registry.get(slug)?, id, Utc::now().timestamp())?
                .ok_or_else(|| {
                    ServicesError::new(
                        404,
                        "service_notification_not_found",
                        "notification not found",
                    )
                })?;
            Ok((200, json!({"notification": entry})))
        }
        _ => Err(method_not_allowed()),
    }
}

fn method_not_allowed() -> ServicesError {
    ServicesError::new(
        405,
        "method_not_allowed",
        "method not allowed for this service notification route",
    )
}

pub(super) fn handle(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    route: Route<'_>,
    owner: bool,
) -> anyhow::Result<()> {
    match process(request, route, owner) {
        Ok((status, value)) => {
            set_extra_response_headers(format!(
                "{}Cache-Control: no-store\r\n",
                extra_response_headers()
            ));
            write_json_response(
                stream,
                status,
                if status == 201 { "Created" } else { "OK" },
                &value,
            )
        }
        Err(error) => {
            let (reason, headers) = match error.status {
                400 => ("Bad Request", ""),
                401 => ("Unauthorized", ""),
                403 => ("Forbidden", ""),
                404 => ("Not Found", ""),
                405 => ("Method Not Allowed", ""),
                409 => ("Conflict", ""),
                410 => ("Gone", ""),
                413 => ("Payload Too Large", ""),
                429 => ("Too Many Requests", "Retry-After: 30\r\n"),
                _ => ("Service Unavailable", ""),
            };
            set_extra_response_headers(format!(
                "{}Cache-Control: no-store\r\n{headers}",
                extra_response_headers()
            ));
            super::server::write_v2_error(
                stream,
                error.status,
                reason,
                error.code,
                &error.message,
                matches!(error.status, 429 | 503),
                json!({}),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_notifications_publish_rejects_owner_bearer_and_web_origin() {
        let mut request = SimpleHttpRequest {
            method: "POST".into(),
            path: "/v2/services/test/notifications".into(),
            headers: [("authorization".into(), "Bearer test-management-root".into())].into(),
            body: vec![],
        };
        let route = || parse_route("/v2/services/test/notifications").unwrap();
        assert_eq!(
            process(&request, route(), true).unwrap_err().code,
            "service_credential_required"
        );
        request.headers.insert(
            "authorization".into(),
            format!("Bearer {}{}", notices::TOKEN_PREFIX, "0".repeat(43)),
        );
        request
            .headers
            .insert("origin".into(), "https://synthetic.invalid".into());
        assert_eq!(
            process(&request, route(), true).unwrap_err().code,
            "service_credential_required"
        );
        request.headers.remove("origin");
        assert_eq!(
            process(&request, Route::History, false).unwrap_err().code,
            "owner_required"
        );
        assert!(parse_route("/v2/services/test/notifications/../settings").is_none());
    }

    #[test]
    fn service_notifications_receiver_requires_a_paired_token_even_for_root() {
        let request = SimpleHttpRequest {
            method: "POST".into(),
            path: receivers::PATH.into(),
            headers: [("authorization".into(), "Bearer synthetic-root".into())].into(),
            body: br#"{"allowed":true}"#.to_vec(),
        };
        let route = || parse_route(receivers::PATH).unwrap();
        assert_eq!(
            process(&request, route(), false).unwrap_err().code,
            "owner_required"
        );
        assert_eq!(
            process(&request, route(), true).unwrap_err().code,
            "android_receiver_pairing_required"
        );
    }

    #[test]
    fn service_notifications_history_returns_hard_limits_only_for_history() {
        let root = std::env::temp_dir().join(format!(
            "cutex-service-notice-history-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&root).unwrap();
        let store = notices::Store::open(&root).unwrap();
        let request = SimpleHttpRequest {
            method: "GET".into(),
            path: HISTORY_PATH.into(),
            headers: Default::default(),
            body: vec![],
        };
        let page = history(&request, &store, 0).unwrap();
        let recovery = history(
            &SimpleHttpRequest {
                path: format!("{HISTORY_PATH}?after=0"),
                ..request
            },
            &store,
            0,
        )
        .unwrap();
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(
            page["retention"],
            json!({"maxAgeSeconds":604800,"maxPerService":200,"maxPerHost":2000})
        );
        assert_eq!(page["nextBefore"], Value::Null);
        assert!(recovery.get("retention").is_none());
    }
}
