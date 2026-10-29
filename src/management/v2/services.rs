//! Management v2 services registry routes (root credential only).
//!
//! - `GET    /v2/services?project=&owner=&health=` → `{hostId, services, settings}`
//!   (`owner=<cutexSessionId>`: services owned by that agent)
//!   (`visibility=all`: include owner-hidden and owner-archived records)
//! - `PATCH  /v2/services` body `{portRangeStart?, portRangeEnd?}` → `{settings}`
//! - `POST   /v2/services` register → `201 {service}`
//! - `GET    /v2/services/{slug}` → `{service}`
//! - `GET    /v2/services/{slug}/eligible-owners` → `{hostId, slug, owners}`
//!   (capability `services-eligible-owners`; all known non-archived sessions)
//! - `PATCH  /v2/services/{slug}` → `{service}`; `ownerCutexSessionId` reassigns
//!   (must name an existing, non-archived cutex session; `null` clears);
//!   `ownerScope: session|project` (capability `services-project-scope`;
//!   project needs a `projectId`, settable in the same request)
//! - `DELETE /v2/services/{slug}` → `{slug, removed: true}`
//! - `POST   /v2/services/{slug}/start|stop|restart` → `{service}`
//! - `POST   /v2/services/{slug}/archive|restore` → `{service}`; archive
//!   stops + hides without deleting files/root, restore leaves managed stopped
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
//! - `GET    /v2/services/{slug}/tile` → `{tile|null, fetchedAt, error|null,
//!   source: static|service}` (capability `service-tiles`)
//! - `GET    /v2/services/{slug}/tile/image` → the tile image (PNG / JPEG /
//!   WebP, at most 512 KiB)
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
use crate::services::store::ServiceRecord;
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
    Archive,
    Restore,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    ViewTicket(String),
    Collection,
    Item(String),
    EligibleOwners(String),
    Action(String, Action),
    Logs(String),
    LogStream(String),
    Lease(String, Option<String>),
    Tile(String),
    TileImage(String),
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
        (Some("eligible-owners"), None, _) => Route::EligibleOwners(slug),
        (Some("start"), None, _) => Route::Action(slug, Action::Start),
        (Some("stop"), None, _) => Route::Action(slug, Action::Stop),
        (Some("restart"), None, _) => Route::Action(slug, Action::Restart),
        (Some("open"), None, _) => Route::Action(slug, Action::Open),
        (Some("archive"), None, _) => Route::Action(slug, Action::Archive),
        (Some("restore"), None, _) => Route::Action(slug, Action::Restore),
        (Some("view-ticket"), None, _) => Route::ViewTicket(slug),
        (Some("lease"), None, _) => Route::Lease(slug, None),
        (Some("lease"), Some(id), None) if !id.is_empty() => Route::Lease(slug, Some(id.into())),
        (Some("logs"), None, _) => Route::Logs(slug),
        (Some("logs"), Some("stream"), None) => Route::LogStream(slug),
        (Some("tile"), None, _) => Route::Tile(slug),
        (Some("tile"), Some("image"), None) => Route::TileImage(slug),
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
    let include_inactive = match query(path, "visibility").as_deref() {
        None | Some("active") => false,
        Some("all") => true,
        Some(_) => {
            return Err(ServicesError::bad_request(
                "visibility must be active or all",
            ))
        }
    };
    Ok(ListFilter {
        project: query(path, "project"),
        owner: query(path, "owner"),
        health,
        include_inactive,
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
    validate_owner_in(&sessions, cutex_session_id)
}

fn validate_owner_in(
    sessions: &crate::session::model::CutexSessionStore,
    cutex_session_id: &str,
) -> ServicesResult<()> {
    crate::role_revision::CutexSessionId::new(cutex_session_id.to_string())
        .map_err(|_| ServicesError::bad_request("invalid ownerCutexSessionId"))?;
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

/// Snapshot projection only. Eligibility shares the assignment validator, not
/// sidebar visibility, agent registration, project membership or runtime state.
fn eligible_owners(
    context: &ServicesContext,
    slug: &str,
    sessions: &crate::session::model::CutexSessionStore,
    projects: &crate::agent_management::AgentManagementSnapshot,
    online: impl Fn(&crate::session::model::CutexSessionRecord) -> bool,
) -> ServicesResult<Value> {
    let service = context.store.get(slug)?;
    let mut owners = Vec::new();
    for (id, record) in &sessions.sessions {
        if validate_owner_in(sessions, id).is_err() {
            continue;
        }
        let memberships = crate::asks::directory::Directory::director_projects(projects, id);
        // A Director can hold several seats. Prefer this service's project;
        // do not invent a single project for an otherwise ambiguous Director.
        let project = service
            .project_id
            .as_ref()
            .filter(|p| memberships.contains(p))
            .cloned()
            .or_else(|| (memberships.len() == 1).then(|| memberships[0].clone()));
        let is_director = service.project_id.as_ref().is_some_and(|project| {
            projects.projects.values().any(|authority| {
                authority.project_id.as_str() == project
                    && authority.authorized_director_session.as_str() == id
                    && crate::memory::project_is_known(projects, &authority.project_id)
            })
        });
        owners.push(json!({
            "id": id,
            "name": crate::session::metadata::cutex_session_presented_name(record),
            "realName": crate::session::metadata::cutex_session_display_name(record),
            "projectId": project,
            "isDirector": is_director,
            "online": online(record),
            "unnamed": record.formal_agent_name.is_none()
                && record.display_name_override.as_deref().is_none_or(|s| s.trim().is_empty())
                && record.registration_class == crate::agent_bus::model::AgentRegistrationClass::LocalOnly,
        }));
    }
    // Stable transport order; the client owns locale sorting and grouping.
    owners.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    Ok(json!({"hostId": context.store.host(), "slug": slug, "owners": owners}))
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

    /// The tile answer of a service (cached, see [`crate::services::tile`]).
    fn tile(&self, slug: &str) -> ServicesResult<(ServiceRecord, Value)> {
        let record = self.store.get(slug)?;
        let fetchable = crate::services::tile::fetchable(&record, self.snapshots().get(slug));
        let scope = self.store.dir().display().to_string();
        let answer = crate::services::tile::global().answer(
            &scope,
            &record,
            fetchable,
            std::time::Instant::now(),
            |path| {
                crate::services::tile::fetch(
                    &record,
                    path,
                    crate::services::tile::MAX_TILE_BYTES,
                    crate::services::tile::FETCH_TIMEOUT,
                    "application/json",
                )
            },
        );
        Ok((record, answer))
    }

    /// The tile image: `(content type, bytes)`.
    fn tile_image(&self, slug: &str) -> ServicesResult<(&'static str, Vec<u8>)> {
        use crate::services::tile;
        let (record, answer) = self.tile(slug)?;
        let Some(image) = answer["tile"]["image"].as_str() else {
            return Err(ServicesError::new(
                404,
                "tile_image_not_found",
                "the service tile has no image",
            ));
        };
        if !tile::fetchable(&record, self.snapshots().get(slug)) {
            return Err(ServicesError::new(
                502,
                "tile_image_unavailable",
                "service_stopped: the service is not running",
            ));
        }
        let bytes = tile::fetch(
            &record,
            image,
            tile::MAX_IMAGE_BYTES,
            tile::FETCH_TIMEOUT,
            "image/png, image/jpeg, image/webp",
        )
        .map_err(|error| {
            ServicesError::new(
                502,
                "tile_image_unavailable",
                format!("{}: {}", error.code, error.message),
            )
        })?;
        let content_type = tile::sniff_image(&bytes).ok_or_else(|| {
            ServicesError::new(
                415,
                "tile_image_unsupported",
                "the tile image must be PNG, JPEG or WebP",
            )
        })?;
        Ok((content_type, bytes))
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
        ("GET", Route::EligibleOwners(slug)) => {
            // Check existence before touching the host-wide session stores.
            context.store.get(slug)?;
            let sessions = crate::session::store::load_cutex_session_store_shared()
                .map_err(ServicesError::unavailable)?;
            let projects = crate::memory::load_agent_management_snapshot()
                .map_err(ServicesError::unavailable)?;
            Ok((
                200,
                eligible_owners(
                    context,
                    slug,
                    &sessions,
                    &projects,
                    crate::session::runtime_intent::record_online,
                )?,
            ))
        }
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
            let value = match action {
                Action::Archive => {
                    context.store.update(
                        slug,
                        UpdateRequest {
                            archived: Some(true),
                            hidden: Some(true),
                            ..Default::default()
                        },
                        &Caller::Owner,
                    )?;
                    context.sync();
                    json!({ "service": context.view(slug)? })
                }
                Action::Restore => {
                    context.store.update(
                        slug,
                        UpdateRequest {
                            archived: Some(false),
                            hidden: Some(false),
                            ..Default::default()
                        },
                        &Caller::Owner,
                    )?;
                    context.sync();
                    json!({ "service": context.view(slug)? })
                }
                Action::Start => json!({ "service": context.supervisor()?.start_service(slug)? }),
                Action::Stop => json!({ "service": context.supervisor()?.stop_service(slug)? }),
                Action::Restart => {
                    json!({ "service": context.supervisor()?.restart_service(slug)? })
                }
                Action::Open => context.supervisor()?.open_service(slug)?,
            };
            Ok((200, value))
        }
        ("GET", Route::Tile(slug)) => Ok((200, context.tile(slug)?.1)),
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
        415 => "Unsupported Media Type",
        502 => "Bad Gateway",
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
    if let Route::TileImage(slug) = &route {
        if request.method != "GET" {
            return write_error(
                stream,
                &ServicesError::new(405, "method_not_allowed", "use GET"),
            );
        }
        return match context.tile_image(slug) {
            Ok((content_type, bytes)) => {
                let previous = crate::http::server::extra_response_headers();
                crate::http::server::set_extra_response_headers(format!(
                    "{previous}Cache-Control: private, max-age=60\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'none'; sandbox\r\n"
                ));
                let result = crate::http::server::write_http_response(
                    stream,
                    200,
                    "OK",
                    content_type,
                    &bytes,
                );
                crate::http::server::set_extra_response_headers(previous);
                result
            }
            Err(error) => write_error(stream, &error),
        };
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
    fn eligible_owner_routes_are_read_only_and_owner_only() {
        let path = "/v2/services/web/eligible-owners";
        assert_eq!(parse_path(path), Some(Route::EligibleOwners("web".into())));
        for bad in [
            "/v2/services/web/eligible-owners/",
            "/v2/services/web/eligible-owners/x",
        ] {
            assert_eq!(parse_path(bad), None);
        }
        assert_eq!(
            super::super::access::classify("GET", path),
            Some(super::super::access::Class::OwnerOnly)
        );
        let context = ServicesContext {
            store: ServiceStore::new(
                std::env::temp_dir().join(uuid::Uuid::new_v4().to_string()),
                "host",
            ),
            supervisor: None,
            owner_check: |_| panic!("a read must not assign an owner"),
            caller: "root".into(),
        };
        let route = parse_path(path).unwrap();
        for method in ["POST", "PATCH", "DELETE"] {
            assert_eq!(
                route_json(&context, method, path, &route, b"{}")
                    .unwrap_err()
                    .status,
                405
            );
        }
        // Missing service is refused before reading any default session store.
        assert_eq!(
            route_json(&context, "GET", path, &route, b"")
                .unwrap_err()
                .status,
            404
        );
    }

    #[test]
    fn eligible_owners_include_hidden_unnamed_offline_and_other_project_sessions() {
        use crate::agent_management::{
            AgentManagementSnapshot, CurrentProjectMembership, ProjectAuthority, ProjectId,
        };
        use crate::session::model::{
            CutexSessionArchiveState, CutexSessionRecord, CutexSessionStore,
        };

        let dir =
            std::env::temp_dir().join(format!("cutex-eligible-owners-{}", uuid::Uuid::new_v4()));
        let context = ServicesContext {
            store: ServiceStore::new(&dir, "host"),
            supervisor: None,
            owner_check: |_| panic!("read must not assign"),
            caller: "root".into(),
        };
        context
            .store
            .register(
                RegisterRequest {
                    slug: "web".into(),
                    project_id: Some("project-a".into()),
                    ..Default::default()
                },
                &Caller::Owner,
            )
            .unwrap();
        let before = context.store.load().unwrap();
        let mut sessions = CutexSessionStore::default();
        for id in [
            "cutex.director",
            "cutex.hidden",
            "cutex.chat",
            "cutex.other",
            "cutex.retired",
            "cutex.retired-at",
        ] {
            let record =
                CutexSessionRecord::new(id.into(), None, "host".into(), "/workspace".into(), None)
                    .unwrap();
            sessions.sessions.insert(id.into(), record);
        }
        let hidden = sessions.sessions.get_mut("cutex.hidden").unwrap();
        hidden.quick_action = crate::session::model::CutexSessionQuickActionMode::Hidden;
        hidden.formal_agent_name = Some("Formal".into());
        hidden.display_name_override = Some(" Alias ".into());
        hidden.agent_enabled = false;
        sessions.sessions.get_mut("cutex.chat").unwrap().thread_name = Some("A chat title".into());
        sessions
            .sessions
            .get_mut("cutex.retired")
            .unwrap()
            .archive_state = CutexSessionArchiveState::Retired;
        sessions
            .sessions
            .get_mut("cutex.retired-at")
            .unwrap()
            .retired_at = Some("2026-10-28T00:00:00Z".into());
        let mut projects = AgentManagementSnapshot::empty();
        let project = ProjectId::new("project-a").unwrap();
        let director = crate::role_revision::CutexSessionId::new("cutex.director").unwrap();
        projects.projects.insert(
            project.clone(),
            ProjectAuthority {
                project_id: project.clone(),
                authorized_director_session: director.clone(),
                authority_epoch: 1,
                updated_at: crate::role_revision::Rfc3339::new("2026-10-28T00:00:00Z").unwrap(),
            },
        );
        let other_project = ProjectId::new("project-b").unwrap();
        projects.projects.insert(
            other_project.clone(),
            ProjectAuthority {
                project_id: other_project.clone(),
                authorized_director_session: director,
                authority_epoch: 1,
                updated_at: crate::role_revision::Rfc3339::new("2026-10-28T00:00:00Z").unwrap(),
            },
        );
        let other = crate::role_revision::CutexSessionId::new("cutex.other").unwrap();
        projects.current_project_memberships.insert(
            other.clone(),
            CurrentProjectMembership {
                cutex_session_id: other,
                project_id: Some(other_project),
                revision: 1,
                updated_at: crate::role_revision::Rfc3339::new("2026-10-28T00:00:00Z").unwrap(),
            },
        );
        let result = eligible_owners(&context, "web", &sessions, &projects, |r| {
            r.cutex_session_id == "cutex.director"
        })
        .unwrap();
        assert_eq!(
            validate_owner_in(&sessions, "cutex.missing")
                .unwrap_err()
                .code,
            "owner_session_not_found"
        );
        assert_eq!(
            validate_owner_in(&sessions, "cutex.retired")
                .unwrap_err()
                .code,
            "owner_session_archived"
        );
        assert_eq!(
            validate_owner_in(&sessions, "invalid owner")
                .unwrap_err()
                .status,
            400
        );
        super::super::schema_export::assert_valid("services", "eligibleOwners", &result);
        let owners = result["owners"].as_array().unwrap();
        assert_eq!(
            owners
                .iter()
                .map(|v| v["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "cutex.chat",
                "cutex.director",
                "cutex.hidden",
                "cutex.other",
                "cutex.retired-at"
            ]
        );
        // A leftover retiredAt alone does not change is_retired(); preserve
        // exactly the assignment validator's lifecycle rule.
        for (id, _) in &sessions.sessions {
            assert_eq!(
                owners.iter().any(|o| o["id"] == id.as_str()),
                validate_owner_in(&sessions, id).is_ok(),
                "{id}"
            );
        }
        assert_eq!(result["hostId"], "host");
        assert_eq!(result["slug"], "web");
        assert_eq!(owners[0]["unnamed"], true);
        assert_eq!(owners[0]["name"], "A chat title");
        assert!(owners[0]["projectId"].is_null());
        assert_eq!(owners[1]["isDirector"], true);
        assert_eq!(owners[1]["projectId"], "project-a");
        assert_eq!(owners[1]["online"], true);
        assert_eq!(owners[2]["name"], "Alias");
        assert_eq!(owners[2]["realName"], "Formal");
        assert_eq!(owners[2]["online"], false);
        assert_eq!(owners[2]["unnamed"], false);
        assert_eq!(owners[3]["projectId"], "project-b");
        assert_eq!(owners[3]["isDirector"], false);
        // A live seat change is reflected in the next response.
        projects
            .projects
            .get_mut(&project)
            .unwrap()
            .authorized_director_session =
            crate::role_revision::CutexSessionId::new("cutex.hidden").unwrap();
        let changed = eligible_owners(&context, "web", &sessions, &projects, |_| false).unwrap();
        assert_eq!(changed["owners"][1]["isDirector"], false);
        assert_eq!(changed["owners"][2]["isDirector"], true);
        assert_eq!(
            serde_json::to_value(context.store.load().unwrap()).unwrap(),
            serde_json::to_value(before).unwrap()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn eligible_owner_schema_requires_typed_metadata() {
        let validator =
            super::super::schema_export::validator("services", "eligibleOwners").unwrap();
        let owner = json!({"id": "cutex.chat", "name": "Chat", "realName": "Chat",
            "projectId": null, "isDirector": false, "online": false, "unnamed": true});
        let answer = json!({"hostId": "host", "slug": "web", "owners": [owner]});
        assert!(validator.is_valid(&answer));
        for field in [
            "id",
            "name",
            "realName",
            "projectId",
            "isDirector",
            "online",
            "unnamed",
        ] {
            let mut missing = answer.clone();
            missing["owners"][0].as_object_mut().unwrap().remove(field);
            assert!(!validator.is_valid(&missing), "{field}");
        }
        let mut invalid = answer;
        invalid["owners"][0]["online"] = json!("offline");
        assert!(!validator.is_valid(&invalid));
    }

    #[test]
    fn eligible_owners_empty_answer_matches_schema_and_capability() {
        let dir =
            std::env::temp_dir().join(format!("cutex-eligible-empty-{}", uuid::Uuid::new_v4()));
        let context = ServicesContext {
            store: ServiceStore::new(&dir, "host"),
            supervisor: None,
            owner_check: |_| Ok(()),
            caller: "root".into(),
        };
        context
            .store
            .register(
                RegisterRequest {
                    slug: "web".into(),
                    ..Default::default()
                },
                &Caller::Owner,
            )
            .unwrap();
        let result = eligible_owners(
            &context,
            "web",
            &Default::default(),
            &crate::agent_management::AgentManagementSnapshot::empty(),
            |_| panic!("empty"),
        )
        .unwrap();
        assert_eq!(result["owners"], json!([]));
        super::super::schema_export::assert_valid("services", "eligibleOwners", &result);
        let caps = super::super::capabilities::capabilities_resource();
        assert_eq!(caps["features"]["servicesEligibleOwners"], true);
        assert!(caps["featureList"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "services-eligible-owners"));
        std::fs::remove_dir_all(dir).unwrap();
    }

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
            parse_path("/v2/services/web/archive"),
            Some(Route::Action("web".into(), Action::Archive))
        );
        assert_eq!(
            parse_path("/v2/services/web/restore"),
            Some(Route::Action("web".into(), Action::Restore))
        );
        assert_eq!(
            parse_path("/v2/services/web/logs"),
            Some(Route::Logs("web".into()))
        );
        assert_eq!(
            parse_path("/v2/services/web/logs/stream"),
            Some(Route::LogStream("web".into()))
        );
        assert_eq!(
            parse_path("/v2/services/web/tile"),
            Some(Route::Tile("web".into()))
        );
        assert_eq!(
            parse_path("/v2/services/web/tile/image"),
            Some(Route::TileImage("web".into()))
        );
        for bad in [
            "/v2/services/",
            "/v2/services/web/tile/x",
            "/v2/services/web/tile/image/x",
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
    fn tile_routes_register_validate_and_serve_images() {
        use std::io::{Read, Write};
        let dir = std::env::temp_dir().join(format!(
            "cutex-services-tiles-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let context = ServicesContext {
            store: ServiceStore::new(&dir, "tilehost"),
            supervisor: None,
            owner_check: |_| Ok(()),
            caller: "root".into(),
        };
        let call = |method: &str, path: &str, body: &str| {
            let route = parse_path(path.split('?').next().unwrap()).unwrap();
            route_json(&context, method, path, &route, body.as_bytes())
        };
        // A fake service: a PNG, an SVG and a live tile.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                let body: Vec<u8> = match head.split(' ').nth(1).unwrap_or("") {
                    "/logo.png" => b"\x89PNG\r\n\x1a\nfake".to_vec(),
                    "/logo.svg" => b"<svg/>".to_vec(),
                    "/live.json" => br#"{"title":"Live","image":"/logo.svg","extra":1}"#.to_vec(),
                    _ => {
                        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                        continue;
                    }
                };
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
                let _ = stream.write_all(&body);
            }
        });
        let (status, created) = call(
            "POST",
            "/v2/services",
            &format!(
                r#"{{"slug":"tiles","port":{port},"tile":{{"title":"Hello","image":"/logo.png","unknown":true}}}}"#
            ),
        )
        .unwrap();
        assert_eq!(status, 201);
        assert_eq!(
            created["service"]["tile"],
            json!({"title": "Hello", "image": "/logo.png"})
        );
        crate::management::v2::schema_export::assert_valid("services", "service", &created["service"]);
        let (_, tile) = call("GET", "/v2/services/tiles/tile", "").unwrap();
        assert_eq!(tile["source"], "static");
        assert_eq!(tile["tile"]["title"], "Hello");
        crate::management::v2::schema_export::assert_valid("services", "tileAnswer", &tile);
        let (content_type, bytes) = context.tile_image("tiles").unwrap();
        assert_eq!(content_type, "image/png");
        assert!(bytes.starts_with(b"\x89PNG"));
        // Invalid templates and paths are refused on register and update.
        for body in [
            r#"{"slug":"bad","tile":{"image":"https://example.com/x.png"}}"#,
            r#"{"slug":"bad","tile":{"actions":[{"label":"Go","path":"javascript:alert(1)"}]}}"#,
            r#"{"slug":"bad","tilePath":"http://example.com/tile.json"}"#,
            r#"{"slug":"bad","tile":"text"}"#,
        ] {
            assert_eq!(call("POST", "/v2/services", body).unwrap_err().code, "invalid_tile", "{body}");
        }
        assert_eq!(
            call("PATCH", "/v2/services/tiles", r#"{"tilePath":"data:text/plain,x"}"#)
                .unwrap_err()
                .code,
            "invalid_tile"
        );
        // A live tile: fetched, validated (unknown field dropped); its SVG
        // image is refused.
        let (_, patched) = call(
            "PATCH",
            "/v2/services/tiles",
            r#"{"tile":null,"tilePath":"/live.json"}"#,
        )
        .unwrap();
        assert!(patched["service"].get("tile").is_none());
        assert_eq!(patched["service"]["tilePath"], "/live.json");
        let (_, tile) = call("GET", "/v2/services/tiles/tile", "").unwrap();
        assert_eq!(tile["source"], "service");
        assert_eq!(tile["tile"], json!({"title": "Live", "image": "/logo.svg"}));
        assert_eq!(tile["error"], Value::Null);
        assert_eq!(context.tile_image("tiles").unwrap_err().code, "tile_image_unsupported");
        // No tile anywhere: null, and no image.
        let (_, cleared) = call("PATCH", "/v2/services/tiles", r#"{"tilePath":null}"#).unwrap();
        assert!(cleared["service"].get("tilePath").is_none());
        let (_, tile) = call("GET", "/v2/services/tiles/tile", "").unwrap();
        assert_eq!((tile["tile"].is_null(), tile["error"].is_null()), (true, true));
        assert_eq!(context.tile_image("tiles").unwrap_err().code, "tile_image_not_found");
        assert_eq!(call("GET", "/v2/services/nope/tile", "").unwrap_err().code, "service_not_found");
        assert_eq!(call("POST", "/v2/services/tiles/tile", "").unwrap_err().status, 405);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn parses_filters_and_tail() {
        let filter =
            parse_filter("/v2/services?project=proj%2Da&owner=cutex.s&health=reachable").unwrap();
        assert_eq!(filter.project.as_deref(), Some("proj-a"));
        assert_eq!(filter.owner.as_deref(), Some("cutex.s"));
        assert_eq!(filter.health, Some(ServiceHealth::Reachable));
        assert!(
            parse_filter("/v2/services?visibility=all")
                .unwrap()
                .include_inactive
        );
        assert!(parse_filter("/v2/services?visibility=hidden").is_err());
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
        assert_eq!(patched["service"]["pinned"], false);
        assert_eq!(patched["service"]["url"], "http://routehost:31234/");
        let (_, pinned) =
            call("PATCH", "/v2/services/web", r#"{"pinned":true}"#).unwrap();
        assert_eq!(pinned["service"]["pinned"], true);
        let (_, organised) = call(
            "PATCH",
            "/v2/services/web",
            r#"{"lifetime":"persistent","purpose":"foreground","groupName":"Reviews","starred":true,"sortOrder":-1,"note":"owner note"}"#,
        )
        .unwrap();
        assert_eq!(organised["service"]["purpose"], "foreground");
        assert_eq!(organised["service"]["groupName"], "Reviews");
        crate::management::v2::schema_export::assert_valid(
            "services",
            "service",
            &organised["service"],
        );
        let (_, archived) = call("POST", "/v2/services/web/archive", "").unwrap();
        assert_eq!(archived["service"]["archived"], true);
        assert!(call("GET", "/v2/services", "").unwrap().1["services"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(
            call("GET", "/v2/services?visibility=all", "").unwrap().1["services"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let (_, restored) = call("POST", "/v2/services/web/restore", "").unwrap();
        assert_eq!(restored["service"]["archived"], false);
        assert_eq!(restored["service"]["hidden"], false);
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
        // services-project-scope: the owner claims it for a project (a
        // projectId is required, settable in the same request) and back.
        assert_eq!(cleared["service"]["ownerScope"], "session");
        assert_eq!(
            call("PATCH", "/v2/services/web", r#"{"ownerScope":"project"}"#)
                .unwrap_err()
                .code,
            "project_scope_unavailable"
        );
        let (_, claimed) = call(
            "PATCH",
            "/v2/services/web",
            &format!(r#"{{"ownerScope":"project","projectId":"proj","ownerCutexSessionId":"{OWNER_OK}"}}"#),
        )
        .unwrap();
        assert_eq!(claimed["service"]["ownerScope"], "project");
        assert_eq!(claimed["service"]["projectId"], "proj");
        crate::management::v2::schema_export::assert_valid("services", "service", &claimed["service"]);
        assert_eq!(
            call("PATCH", "/v2/services/web", r#"{"projectId":null}"#)
                .unwrap_err()
                .code,
            "project_scope_unavailable"
        );
        assert_eq!(
            call("PATCH", "/v2/services/web", r#"{"ownerScope":"team"}"#)
                .unwrap_err()
                .code,
            "invalid_request"
        );
        let (_, personal) =
            call("PATCH", "/v2/services/web", r#"{"ownerScope":"session","ownerCutexSessionId":null}"#)
                .unwrap();
        assert_eq!(personal["service"]["ownerScope"], "session");
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
