//! Management v2 services registry routes (root credential only).
//!
//! - `GET    /v2/services?project=&owner=&health=` → `{hostId, services, settings}`
//!   (`owner=<cutexSessionId>`: services owned by that agent)
//! - `PATCH  /v2/services` body `{portRangeStart?, portRangeEnd?}` → `{settings}`
//! - `POST   /v2/services` register → `201 {service}`
//! - `GET    /v2/services/{slug}` → `{service}`
//! - `PATCH  /v2/services/{slug}` → `{service}`; `ownerCutexSessionId` reassigns
//!   (must name an existing, non-archived cutex session; `null` clears)
//! - `DELETE /v2/services/{slug}` → `{slug, removed: true}`
//! - `POST   /v2/services/{slug}/start|stop|restart` → `{service}`
//! - `POST   /v2/services/{slug}/open` → `{slug, url, localUrl, health, service}`;
//!   a stopped managed service is started first
//! - `POST   /v2/services/{slug}/view-ticket` → `{ticket, expiresAt, hostLabel, authPath}`
//! - `POST   /v2/services/{slug}/lease` `{holder?, clientLeaseId?}` → `201 {leaseId,
//!   expiresAt, ttlSeconds}`; a repeated `clientLeaseId` from the same caller
//!   renews and returns the live lease it opened (capability
//!   `services-lease-idempotent`)
//! - `POST|DELETE /v2/services/{slug}/lease/{leaseId}` renews/releases a lease
//! - `GET    /v2/services?tombstones=1` also returns retained tombstones
//! - `GET    /v2/services/{slug}/logs?tail=` → `{slug, text, offset, source}`
//! - `GET    /v2/services/{slug}/logs/stream?tail=` (SSE)
//!
//! Changes and health flips are published as `cutex/services/changed` on the
//! host event stream under the reserved `cutexSessionId` [`HOST_EVENT_SESSION_ID`].

use std::net::TcpStream;
use std::sync::Arc;

use serde_json::json;
use serde_json::Value;

use crate::http::server::write_json_response;
use crate::http::server::SimpleHttpRequest;
use crate::services::health::ServiceHealth;
use crate::services::store::list_views;
use crate::services::store::service_view;
use crate::services::store::Caller;
use crate::services::store::ListFilter;
use crate::services::store::RegisterRequest;
use crate::services::store::ServiceStore;
use crate::services::store::ServicesError;
use crate::services::store::ServicesResult;
use crate::services::store::SettingsPatch;
use crate::services::store::UpdateRequest;
use crate::services::supervisor::Supervisor;

pub const SERVICES_CHANGED_EVENT: &str = "cutex/services/changed";
/// Host-scoped events (not about one agent) use this pseudo session id.
pub const HOST_EVENT_SESSION_ID: &str = "cutex.host";

pub fn publish_reveal(params: Value) -> anyhow::Result<()> {
    super::server::append_cutex_event(
        super::repository::management_v2_repository()?,
        HOST_EVENT_SESSION_ID,
        super::model::EventCorrelation::default(),
        "cutex/services/revealRequested",
        params,
    )
}
const DEFAULT_LOG_TAIL: usize = 200;
const MAX_LOG_TAIL: usize = 5000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Start,
    Stop,
    Restart,
    Open,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    ViewTicket(String),
    Collection,
    Item(String),
    Action(String, Action),
    Logs(String),
    LogStream(String),
    Lease(String, Option<String>),
}

pub fn parse_path(path: &str) -> Option<Route> {
    if path == "/v2/services" {
        return Some(Route::Collection);
    }
    let rest = path.strip_prefix("/v2/services/")?;
    let mut parts = rest.split('/');
    let slug = parts.next().filter(|slug| !slug.is_empty())?.to_string();
    let route = match (parts.next(), parts.next(), parts.next()) {
        (None, _, _) => Route::Item(slug),
        (Some("start"), None, _) => Route::Action(slug, Action::Start),
        (Some("stop"), None, _) => Route::Action(slug, Action::Stop),
        (Some("restart"), None, _) => Route::Action(slug, Action::Restart),
        (Some("open"), None, _) => Route::Action(slug, Action::Open),
        (Some("view-ticket"), None, _) => Route::ViewTicket(slug),
        (Some("lease"), None, _) => Route::Lease(slug, None),
        (Some("lease"), Some(id), None) if !id.is_empty() => Route::Lease(slug, Some(id.into())),
        (Some("logs"), None, _) => Route::Logs(slug),
        (Some("logs"), Some("stream"), None) => Route::LogStream(slug),
        _ => return None,
    };
    Some(route)
}

pub fn is_services_path(path: &str) -> bool {
    path == "/v2/services" || path.starts_with("/v2/services/")
}

fn percent_decode(value: &str) -> Option<String> {
    let value = value.replace('+', " ");
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = value.get(index + 1..index + 3)?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn query(path: &str, key: &str) -> Option<String> {
    crate::http::query::query_value(path, key)
        .and_then(|value| percent_decode(&value))
        .filter(|value| !value.is_empty())
}

pub fn parse_filter(path: &str) -> ServicesResult<ListFilter> {
    let health = match query(path, "health") {
        None => None,
        Some(value) => Some(ServiceHealth::parse(&value).ok_or_else(|| {
            ServicesError::bad_request("health must be reachable, unreachable, starting or stopped")
        })?),
    };
    Ok(ListFilter {
        project: query(path, "project"),
        owner: query(path, "owner"),
        health,
    })
}

pub fn parse_tail(path: &str) -> ServicesResult<usize> {
    match query(path, "tail") {
        None => Ok(DEFAULT_LOG_TAIL),
        Some(value) => value
            .parse::<usize>()
            .map(|tail| tail.min(MAX_LOG_TAIL))
            .map_err(|_| ServicesError::bad_request("tail must be a non-negative integer")),
    }
}

fn parse_body<T: for<'de> serde::Deserialize<'de>>(body: &[u8]) -> ServicesResult<T> {
    let body = if body.iter().all(u8::is_ascii_whitespace) {
        b"{}".as_slice()
    } else {
        body
    };
    serde_json::from_slice(body)
        .map_err(|error| ServicesError::bad_request(format!("invalid request body: {error}")))
}

fn supervisor_required() -> ServicesResult<Arc<Supervisor>> {
    crate::services::supervisor::global().ok_or_else(|| {
        ServicesError::new(
            503,
            "services_supervisor_unavailable",
            "the services supervisor is not running in this daemon",
        )
    })
}

/// An owner the desktop assigns must be a known, non-archived cutex session.
pub fn validate_owner_session(cutex_session_id: &str) -> ServicesResult<()> {
    crate::role_revision::CutexSessionId::new(cutex_session_id.to_string())
        .map_err(|_| ServicesError::bad_request("invalid ownerCutexSessionId"))?;
    let sessions =
        crate::session::store::load_cutex_session_store().map_err(ServicesError::unavailable)?;
    match sessions.sessions.get(cutex_session_id) {
        None => Err(ServicesError::new(
            404,
            "owner_session_not_found",
            format!("no cutex session `{cutex_session_id}`"),
        )),
        Some(record) if record.is_retired() => Err(ServicesError::new(
            409,
            "owner_session_archived",
            format!("cutex session `{cutex_session_id}` is archived"),
        )),
        Some(_) => Ok(()),
    }
}

/// The store plus (when running) the daemon's supervisor.
pub struct ServicesContext {
    pub store: ServiceStore,
    pub supervisor: Option<Arc<Supervisor>>,
    /// Checks an owner set through the desktop routes.
    pub owner_check: fn(&str) -> ServicesResult<()>,
    /// The calling credential (client / device token id, or `root`): the
    /// scope of a lease open's `clientLeaseId`.
    pub caller: String,
}

/// The calling credential of the current request.
fn current_caller() -> String {
    crate::management::listeners::current_client_token()
        .map(|token| token.token_id)
        .unwrap_or_else(|| crate::management::persons::ROOT_TOKEN_ID.to_owned())
}

impl ServicesContext {
    pub fn default_context() -> ServicesResult<Self> {
        match crate::services::supervisor::global() {
            Some(supervisor) => Ok(Self {
                store: supervisor.store().clone(),
                supervisor: Some(supervisor),
                owner_check: validate_owner_session,
                caller: current_caller(),
            }),
            None => Ok(Self {
                store: ServiceStore::open_default().map_err(ServicesError::unavailable)?,
                supervisor: None,
                owner_check: validate_owner_session,
                caller: current_caller(),
            }),
        }
    }

    fn snapshots(
        &self,
    ) -> std::collections::BTreeMap<String, crate::services::health::RuntimeSnapshot> {
        match &self.supervisor {
            Some(supervisor) => supervisor.snapshots(),
            None => self.store.load_state(),
        }
    }

    fn view(&self, slug: &str) -> ServicesResult<Value> {
        if let Some(supervisor) = &self.supervisor {
            supervisor.sync();
            if let Ok(view) = supervisor.view(slug) {
                return Ok(view);
            }
        }
        let record = self.store.get(slug)?;
        let mut view = service_view(&record, self.store.load_state().get(slug));
        let traffic = crate::services::traffic::snapshot_for(&record);
        crate::services::lifecycle::for_store(&self.store).augment_view(
            &record,
            &self.store.load()?.settings,
            traffic.last_traffic_at.as_deref(),
            traffic.active_connections,
            &mut view,
        );
        Ok(view)
    }

    fn sync(&self) {
        if let Some(supervisor) = &self.supervisor {
            supervisor.sync();
        }
    }

    fn supervisor(&self) -> ServicesResult<Arc<Supervisor>> {
        self.supervisor
            .clone()
            .map(Ok)
            .unwrap_or_else(supervisor_required)
    }
}

/// Every JSON route (SSE excluded): `(status, body)`.
pub fn route_json(
    context: &ServicesContext,
    method: &str,
    path_with_query: &str,
    route: &Route,
    body: &[u8],
) -> ServicesResult<(u16, Value)> {
    match (method, route) {
        ("POST", Route::ViewTicket(slug)) => {
            #[derive(serde::Deserialize, Default)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct Request {
                path: Option<String>,
                start: Option<bool>,
                routing: Option<String>,
                /// `loginLinkCommand` services: run it (default: only when `path` is the
                /// service's own entry path).
                login: Option<bool>,
            }
            let request: Request = parse_body(body)?;
            if request
                .routing
                .as_deref()
                .is_some_and(|routing| routing != "host")
            {
                return Err(ServicesError::new(
                    400,
                    "routing_unsupported",
                    "only host routing is supported",
                ));
            }
            let record = context.store.get(slug)?;
            if request.start == Some(true)
                && record.runtime == crate::services::ServiceRuntime::Managed
            {
                context.supervisor()?.open_service(slug)?;
            }
            let entry = record.path.as_deref().unwrap_or("/");
            let mut next = request.path.as_deref().unwrap_or(entry).to_string();
            let mut login = None;
            if record.login_link_command.is_some()
                && record.runtime == crate::services::ServiceRuntime::Managed
                && request.login.unwrap_or(next == entry)
            {
                login = Some(match context.supervisor()?.login_link(&record) {
                    None => "notRunning",
                    Some(Ok(path)) => {
                        next = path;
                        "issued"
                    }
                    Some(Err(error)) => error.label(),
                });
            }
            let mut ticket = crate::services::proxy::auth::issue(&context.store, slug, &next)?;
            if let Some(login) = login {
                ticket["login"] = serde_json::json!(login);
            }
            Ok((200, ticket))
        }
        ("GET", Route::Collection) => {
            let filter = parse_filter(path_with_query)?;
            if let Some(supervisor) = &context.supervisor {
                supervisor.sync();
            }
            let registry = context.store.load()?;
            let mut services = list_views(&registry, &context.snapshots(), &filter);
            let life = crate::services::lifecycle::for_store(&context.store);
            for view in &mut services {
                if let Some(record) = view["slug"]
                    .as_str()
                    .and_then(|slug| registry.services.get(slug))
                {
                    let traffic = crate::services::traffic::snapshot_for(record);
                    life.augment_view(
                        record,
                        &registry.settings,
                        traffic.last_traffic_at.as_deref(),
                        traffic.active_connections,
                        view,
                    );
                }
            }
            let mut value = json!({"hostId": context.store.host(),"services":services,"settings":registry.settings});
            if query(path_with_query, "tombstones").as_deref() == Some("1") {
                value["tombstones"] = json!(registry.tombstones.values().collect::<Vec<_>>());
            }
            Ok((200, value))
        }
        ("PATCH", Route::Collection) => {
            let patch: SettingsPatch = parse_body(body)?;
            let settings = context.store.update_settings(patch)?;
            context.sync();
            Ok((200, json!({ "settings": settings })))
        }
        ("POST", Route::Collection) => {
            let request: RegisterRequest = parse_body(body)?;
            if let Some(owner) = request.owner_cutex_session_id.as_deref() {
                (context.owner_check)(owner)?;
            }
            let (record, _) = context.store.register(request, &Caller::Owner)?;
            context.sync();
            Ok((201, json!({ "service": context.view(&record.slug)? })))
        }
        ("GET", Route::Item(slug)) => {
            if let Some(tomb) = context.store.get_tombstone(slug)? {
                return Ok((
                    410,
                    json!({"contractVersion":2,"error":{"source":"cutex","code":"service_reclaimed","message":"service was reclaimed","retryable":false,"details":{"reason":tomb.reason,"reclaimedAt":tomb.reclaimed_at}}}),
                ));
            }
            Ok((200, json!({ "service": context.view(slug)? })))
        }
        ("POST", Route::Lease(slug, id)) => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Body {
                holder: Option<String>,
                /// Idempotency key of a new lease (capability
                /// `services-lease-idempotent`), scoped to the caller.
                #[serde(rename = "clientLeaseId")]
                client_lease_id: Option<String>,
            }
            let request: Body = parse_body(body)?;
            let life = crate::services::lifecycle::for_store(&context.store);
            let lease = context.store.mutate(|registry| {
                if !registry.services.contains_key(slug) {
                    return Err(ServicesError::not_found(slug));
                }
                life.lease_keyed(
                    slug,
                    id.as_deref(),
                    request.holder.as_deref(),
                    request.client_lease_id.as_deref().map(|client_lease_id| {
                        crate::services::lifecycle::ClientLeaseKey {
                            caller: &context.caller,
                            client_lease_id,
                        }
                    }),
                    chrono::Utc::now(),
                )
            })?;
            Ok((if id.is_some() { 200 } else { 201 }, json!(lease)))
        }
        ("DELETE", Route::Lease(slug, Some(id))) => Ok((
            200,
            json!({"released":crate::services::lifecycle::for_store(&context.store).release(slug,id,chrono::Utc::now())}),
        )),
        ("PATCH", Route::Item(slug)) => {
            let request: UpdateRequest = parse_body(body)?;
            if let Some(Some(owner)) = request.owner_cutex_session_id.as_ref() {
                (context.owner_check)(owner)?;
            }
            context.store.update(slug, request, &Caller::Owner)?;
            context.sync();
            Ok((200, json!({ "service": context.view(slug)? })))
        }
        ("DELETE", Route::Item(slug)) => {
            context.store.remove(slug, &Caller::Owner)?;
            context.sync();
            Ok((200, json!({ "slug": slug, "removed": true })))
        }
        ("POST", Route::Action(slug, action)) => {
            let supervisor = context.supervisor()?;
            let value = match action {
                Action::Start => json!({ "service": supervisor.start_service(slug)? }),
                Action::Stop => json!({ "service": supervisor.stop_service(slug)? }),
                Action::Restart => json!({ "service": supervisor.restart_service(slug)? }),
                Action::Open => supervisor.open_service(slug)?,
            };
            Ok((200, value))
        }
        ("GET", Route::Logs(slug)) => {
            let tail = parse_tail(path_with_query)?;
            let supervisor = context.supervisor()?;
            supervisor.sync();
            Ok((200, supervisor.logs(slug, tail)?))
        }
        _ => Err(ServicesError::new(
            405,
            "method_not_allowed",
            format!("{method} is not supported on this services route"),
        )),
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        410 => "Gone",
        _ => "Service Unavailable",
    }
}

fn write_error(stream: &mut TcpStream, error: &ServicesError) -> anyhow::Result<()> {
    super::server::write_v2_error(
        stream,
        error.status,
        reason(error.status),
        error.code,
        &error.message,
        error.status == 503,
        json!({}),
    )
}

/// Entry point after the root credential check.
pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let Some(route) = parse_path(path) else {
        return write_error(
            stream,
            &ServicesError::new(404, "route_not_found", "unknown services route"),
        );
    };
    let context = match ServicesContext::default_context() {
        Ok(context) => context,
        Err(error) => return write_error(stream, &error),
    };
    if let Route::LogStream(slug) = &route {
        if request.method != "GET" {
            return write_error(
                stream,
                &ServicesError::new(405, "method_not_allowed", "use GET"),
            );
        }
        let tail = match parse_tail(&request.path) {
            Ok(tail) => tail,
            Err(error) => return write_error(stream, &error),
        };
        let supervisor = match context.supervisor() {
            Ok(supervisor) => supervisor,
            Err(error) => return write_error(stream, &error),
        };
        supervisor.sync();
        return supervisor.stream_logs(stream, slug, tail);
    }
    match route_json(
        &context,
        &request.method,
        &request.path,
        &route,
        &request.body,
    ) {
        Ok((status, body)) => write_json_response(stream, status, reason(status), &body),
        Err(error) => write_error(stream, &error),
    }
}

/// Start the daemon's services supervisor, publishing its events on the
/// Management v2 host event stream.
pub fn start_supervisor() -> anyhow::Result<()> {
    crate::services::supervisor::start_global(Box::new(|event| {
        let Ok(repository) = super::repository::management_v2_repository() else {
            return;
        };
        if let Err(error) = super::server::append_cutex_event(
            repository,
            HOST_EVENT_SESSION_ID,
            super::model::EventCorrelation::default(),
            SERVICES_CHANGED_EVENT,
            event.params(),
        ) {
            eprintln!("services: could not publish event: {error:#}");
        }
    }))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_routes() {
        assert_eq!(parse_path("/v2/services"), Some(Route::Collection));
        assert_eq!(
            parse_path("/v2/services/web"),
            Some(Route::Item("web".into()))
        );
        assert_eq!(
            parse_path("/v2/services/web/start"),
            Some(Route::Action("web".into(), Action::Start))
        );
        assert_eq!(
            parse_path("/v2/services/web/open"),
            Some(Route::Action("web".into(), Action::Open))
        );
        assert_eq!(
            parse_path("/v2/services/web/logs"),
            Some(Route::Logs("web".into()))
        );
        assert_eq!(
            parse_path("/v2/services/web/logs/stream"),
            Some(Route::LogStream("web".into()))
        );
        for bad in [
            "/v2/services/",
            "/v2/services/web/bogus",
            "/v2/services/web/logs/x",
            "/v2/services/web/start/x",
            "/v2/servicesx",
        ] {
            assert_eq!(parse_path(bad), None, "{bad}");
        }
        assert!(is_services_path("/v2/services/web/x"));
        assert!(!is_services_path("/v2/sessions"));
    }

    #[test]
    fn lease_routes_and_tombstone_details_match_schema() {
        let dir =
            std::env::temp_dir().join(format!("cutex-service-lease-api-{}", uuid::Uuid::new_v4()));
        let store = ServiceStore::new(dir.join("services"), "host");
        let (record, _) = store
            .register(
                RegisterRequest {
                    slug: "web".into(),
                    ..Default::default()
                },
                &Caller::Owner,
            )
            .unwrap();
        let context = ServicesContext {
            store: store.clone(),
            supervisor: None,
            owner_check: |_| Ok(()),
            caller: "root".into(),
        };
        let route = parse_path("/v2/services/web/lease").unwrap();
        let (status, lease) = route_json(&context, "POST", "", &route, b"{}").unwrap();
        assert_eq!(status, 201);
        crate::management::v2::schema_export::assert_valid("services", "lease", &lease);
        let renew = Route::Lease(
            "web".into(),
            Some(lease["leaseId"].as_str().unwrap().into()),
        );
        assert_eq!(
            route_json(&context, "POST", "", &renew, b"").unwrap().0,
            200
        );
        assert_eq!(
            route_json(&context, "DELETE", "", &renew, b"").unwrap().1["released"],
            true
        );
        assert_eq!(
            route_json(&context, "DELETE", "", &renew, b"").unwrap().1["released"],
            false
        );
        // clientLeaseId: a retried open returns the same lease, no second one.
        let keyed = br#"{"clientLeaseId":"5f0c6a52-0000-4000-8000-000000000001"}"#;
        let (status, first) = route_json(&context, "POST", "", &route, keyed).unwrap();
        assert_eq!(status, 201);
        crate::management::v2::schema_export::assert_valid("services", "lease", &first);
        let (status, retry) = route_json(&context, "POST", "", &route, keyed).unwrap();
        assert_eq!(status, 201);
        assert_eq!(retry["leaseId"], first["leaseId"]);
        let life = crate::services::lifecycle::for_store(&store);
        assert_eq!(life.facts("web", chrono::Utc::now()).0, 1);
        // Another credential with the same key does not collide.
        let other = ServicesContext {
            store: store.clone(),
            supervisor: None,
            owner_check: |_| Ok(()),
            caller: "client-token-2".into(),
        };
        let (_, theirs) = route_json(&other, "POST", "", &route, keyed).unwrap();
        assert_ne!(theirs["leaseId"], first["leaseId"]);
        assert_eq!(life.facts("web", chrono::Utc::now()).0, 2);
        // No key keeps the old behaviour: each open is a new lease.
        let (_, a) = route_json(&context, "POST", "", &route, b"{}").unwrap();
        let (_, b) = route_json(&context, "POST", "", &route, b"").unwrap();
        assert_ne!(a["leaseId"], b["leaseId"]);
        assert_eq!(life.facts("web", chrono::Utc::now()).0, 4);
        assert_eq!(
            route_json(&context, "POST", "", &route, br#"{"clientLeaseId":""}"#)
                .unwrap_err()
                .status,
            400
        );
        store
            .mutate(|registry| {
                registry.services.remove("web");
                registry.tombstones.insert(
                    "web".into(),
                    crate::services::lifecycle::tombstone(&record, "idle", chrono::Utc::now()),
                );
                Ok(())
            })
            .unwrap();
        let (status, value) =
            route_json(&context, "GET", "", &Route::Item("web".into()), b"").unwrap();
        assert_eq!(status, 410);
        assert_eq!(value["error"]["details"]["reason"], "idle");
        crate::management::v2::schema_export::assert_valid("error", "errorBody", &value);
        let (_, list) = route_json(
            &context,
            "GET",
            "/v2/services?tombstones=1",
            &Route::Collection,
            b"",
        )
        .unwrap();
        assert_eq!(list["tombstones"].as_array().unwrap().len(), 1);
        crate::management::v2::schema_export::assert_valid("services", "serviceList", &list);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn services_event_matches_the_method_schema() {
        for event in [
            crate::services::supervisor::ServiceEvent {
                reason: "health",
                slug: Some("web".into()),
                service: Some(json!({"slug": "web", "health": "reachable"})),
            },
            crate::services::supervisor::ServiceEvent {
                reason: "settings",
                slug: None,
                service: None,
            },
        ] {
            super::super::contract_validation::validate_cutex_event_message(
                &json!({"method": SERVICES_CHANGED_EVENT, "params": event.params()}),
            )
            .unwrap();
        }
        assert!(
            super::super::contract_validation::validate_cutex_event_message(
                &json!({"method": SERVICES_CHANGED_EVENT, "params": {"reason": "bogus"}}),
            )
            .is_err()
        );
    }

    #[test]
    fn parses_filters_and_tail() {
        let filter =
            parse_filter("/v2/services?project=proj%2Da&owner=cutex.s&health=reachable").unwrap();
        assert_eq!(filter.project.as_deref(), Some("proj-a"));
        assert_eq!(filter.owner.as_deref(), Some("cutex.s"));
        assert_eq!(filter.health, Some(ServiceHealth::Reachable));
        assert!(parse_filter("/v2/services?health=bogus").is_err());
        assert_eq!(parse_tail("/v2/services/a/logs").unwrap(), DEFAULT_LOG_TAIL);
        assert_eq!(parse_tail("/v2/services/a/logs?tail=10").unwrap(), 10);
        assert_eq!(
            parse_tail("/v2/services/a/logs?tail=999999").unwrap(),
            MAX_LOG_TAIL
        );
        assert!(parse_tail("/v2/services/a/logs?tail=-1").is_err());
    }

    #[test]
    fn json_routes_against_a_store() {
        let dir = std::env::temp_dir().join(format!(
            "cutex-services-routes-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fn owner_check(id: &str) -> ServicesResult<()> {
            match id {
                OWNER_OK => Ok(()),
                OWNER_ARCHIVED => Err(ServicesError::new(
                    409,
                    "owner_session_archived",
                    "archived",
                )),
                _ => Err(ServicesError::new(
                    404,
                    "owner_session_not_found",
                    "missing",
                )),
            }
        }
        const OWNER_OK: &str = "cutex.00000000-0000-0000-0000-000000000001";
        const OWNER_ARCHIVED: &str = "cutex.00000000-0000-0000-0000-000000000002";
        let context = ServicesContext {
            store: ServiceStore::new(&dir, "routehost"),
            supervisor: None,
            owner_check,
            caller: "root".into(),
        };
        let call = |method: &str, path: &str, body: &str| {
            let route = parse_path(path.split('?').next().unwrap()).unwrap();
            route_json(&context, method, path, &route, body.as_bytes())
        };
        let (status, created) = call(
            "POST",
            "/v2/services",
            r#"{"slug":"web","port":31234,"path":"/health","displayName":"Web"}"#,
        )
        .unwrap();
        assert_eq!(status, 201);
        assert_eq!(created["service"]["url"], "http://routehost:31234/health");
        assert_eq!(created["service"]["runtime"], "external");
        let (_, listed) = call("GET", "/v2/services", "").unwrap();
        assert_eq!(listed["services"].as_array().unwrap().len(), 1);
        assert_eq!(listed["settings"]["portRangeStart"], 24400);
        let (_, patched) = call("PATCH", "/v2/services/web", r#"{"path":null}"#).unwrap();
        assert!(patched["service"].get("pinned").is_none());
        assert_eq!(patched["service"]["url"], "http://routehost:31234/");
        assert_eq!(
            call("PATCH", "/v2/services/web", r#"{"pinned":true}"#)
                .unwrap_err()
                .code,
            "invalid_request"
        );
        // Owner reassignment: validated, filterable, clearable.
        let (_, owned) = call(
            "PATCH",
            "/v2/services/web",
            &format!(r#"{{"ownerCutexSessionId":"{OWNER_OK}"}}"#),
        )
        .unwrap();
        assert_eq!(owned["service"]["ownerCutexSessionId"], OWNER_OK);
        let (_, by_owner) = call("GET", &format!("/v2/services?owner={OWNER_OK}"), "").unwrap();
        assert_eq!(by_owner["services"].as_array().unwrap().len(), 1);
        let (_, by_other) =
            call("GET", &format!("/v2/services?owner={OWNER_ARCHIVED}"), "").unwrap();
        assert!(by_other["services"].as_array().unwrap().is_empty());
        assert_eq!(
            call(
                "PATCH",
                "/v2/services/web",
                &format!(r#"{{"ownerCutexSessionId":"{OWNER_ARCHIVED}"}}"#),
            )
            .unwrap_err()
            .code,
            "owner_session_archived"
        );
        assert_eq!(
            call(
                "POST",
                "/v2/services",
                r#"{"slug":"api","ownerCutexSessionId":"cutex.00000000-0000-0000-0000-000000000009"}"#,
            )
            .unwrap_err()
            .code,
            "owner_session_not_found"
        );
        let (_, cleared) = call(
            "PATCH",
            "/v2/services/web",
            r#"{"ownerCutexSessionId":null}"#,
        )
        .unwrap();
        assert_eq!(cleared["service"]["ownerCutexSessionId"], Value::Null);
        assert_eq!(
            call("POST", "/v2/services/web/start", "").unwrap_err().code,
            "services_supervisor_unavailable"
        );
        assert_eq!(
            call("POST", "/v2/services", r#"{"slug":"x","nope":1}"#)
                .unwrap_err()
                .code,
            "invalid_request"
        );
        assert_eq!(
            call("PUT", "/v2/services/web", "{}").unwrap_err().status,
            405
        );
        let (_, settings) = call("PATCH", "/v2/services", r#"{"portRangeEnd":30000}"#).unwrap();
        assert_eq!(settings["settings"]["portRangeEnd"], 30000);
        assert!(settings["settings"].get("idleTimeoutMinutes").is_none());
        assert!(call("PATCH", "/v2/services", r#"{"idleTimeoutMinutes":5}"#).is_err());
        let (_, removed) = call("DELETE", "/v2/services/web", "").unwrap();
        assert_eq!(removed["removed"], true);
        assert_eq!(
            call("GET", "/v2/services/web", "").unwrap_err().code,
            "service_not_found"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
