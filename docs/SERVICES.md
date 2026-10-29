# Services: authenticated proxy, lifetime and static content

The Management v2 services routes require the owner credential (authorized
client/device tokens keep their existing owner mapping). Agent Bus service
tools use the authenticated agent's durable session and the existing project
visibility boundary. No caller identity is accepted from tool arguments.

## Eligible owners (capability `services-eligible-owners`)

`GET /v2/services/{slug}/eligible-owners` returns
`{hostId, slug, owners: [{id, name, realName, projectId, isDirector, online, unnamed}]}`.
Clients check `featureList: services-eligible-owners` or
`features.servicesEligibleOwners` before requesting it. This uses the existing
owner-authorized Management services guard, including authorized client/device
tokens; it is not an Agent Bus/MCP discovery API and accepts no caller identity.

The list covers every known session accepted by the owner's assignment
validator, including sessions absent from the sidebar, hidden sessions, local
unnamed chats, offline agents and agents in other projects. It excludes retired
sessions using the same lifecycle predicate as `PATCH ownerCutexSessionId`.
Eligibility does not depend on runtime state, agent enablement or registration
class. `name` uses the owner's display alias before the normal display-name
chain; `realName` uses the same chain without the alias. `unnamed` identifies
local-only chats with neither a formal agent name nor a nonempty display alias.

`projectId` is the current roster project or Director seat, preferring this
service's project when a Director has multiple seats; otherwise an ambiguous
Director has `null`. `isDirector` means the current Director of **this service's
project**, for project-first picker ordering. Both are presentation hints, not
assignment restrictions. `online` uses the existing runtime-intent process
liveness projection (binding present and not proven gone, including boot/PID
reuse checks); it does not assert a connected native control channel. Offline
sessions remain fully eligible. Rows have stable ID order; locale sorting,
offline folding and hiding unnamed chats until searched belong to the client.

The service must exist (404 `service_not_found`, or 410 `service_reclaimed` for
a retained tombstone). Methods other than GET return 405 `method_not_allowed`.
Unavailable session/project stores return 503 rather than a misleading empty
list. The response is an unpaginated snapshot, not an assignment reservation:
the existing PATCH validation still runs if a session is archived or removed
between listing and reassignment. No service, session or runtime is changed.

## Project ownership (capability `services-project-scope`)

`ownerScope` is `session` or `project`; missing fields in older records mean
`session`. `ownerCutexSessionId` remains the registering session. A project
service also requires `projectId`. Its current Director and its registrant
while still in that project may manage it, as may the owner. Director changes
take effect on the next operation; a session service keeps its existing
registrant-only management rule. Agent lists include `ownerScope` and
`youMayManage`, and show process details only to agents allowed to manage them.
`loginLinkCommand` remains owner only.

`service_register` accepts optional `scope: session|project`. A Director of a
single resolved project defaults new services to project scope; other callers
default to session scope. Explicit project scope requires a current project;
an explicit project request with an ambiguous Director context is refused. Agents cannot supply an owner
or a project ID. `service_update` can promote a service of the caller's current
project with `ownerScope: project`; only the owner can demote it to session.
The owner's `PATCH /v2/services/{slug}` accepts `ownerScope` and `projectId`
together, including conversion of legacy records without a project.

Managing another member's service uses the caller's own execution and approval
policy. Removing a persistent project service registered by someone else
requires owner confirmation; the same confirmation applies when releasing
that service's persistent retention. Final writes revalidate the caller under
the roster mutation fence and reject records or project context changed while
an approval was pending. Archiving or deleting the registering session
does not reclaim project services; deleting the session clears their
registrant field. Ephemeral project services instead follow project archive
and the existing TTL/idle/lease rules. Persistent services retain the existing
owner-controlled lifecycle. Older daemons ignore project ownership fields and
apply their session management rules; clients must check the capability before
offering conversion.

`cutex/project/directorChanged` adds optional `projectServiceCount` (null if the
registry could not be read). It counts that project's project services and
provides a UI hint; the current list remains the source for service details.

## Host routing and authentication

Only loopback Management connections route `<slug>.svc.localhost:<P>` to
services; SSH local forwarding works because sshd connects from loopback. The
port may be any tunnel port. Tailnet connections retain Management authentication.
Service hosts never dispatch to Management routes and remain outside its CORS
allowlist. Changing Host mid-connection returns 421.

Desktop main calls `POST /v2/services/{slug}/view-ticket` with optional
`{path, start, routing:"host"}`. The response is
`{ticket, expiresAt, hostLabel, authPath}`; navigate to
`http://<hostLabel>:<P><authPath>`. Other routing values return 400
`routing_unsupported`. With `start:true`, managed startup behaves like `/open`.
The proxy itself never starts a stopped service.

Login links (capability `services-login-link`): an owner-registered managed
service may carry `loginLinkCommand` (shell line or argv, `{port}` substituted,
same cwd / env as `command`; agents get 403 `owner_only`; other runtimes 400).
For services that log a browser in with a one-time URL (Claude Science
`claude-science url`, Jupyter-style tokens), a view ticket runs it when the
service is running and the request asks for the service's entry path (its
`path`, default `/`) or sets `login: true` (`login: false` skips it). The last
stdout line that is an `http(s)` URL on `localhost` / `127.0.0.1` / `[::1]` with
the service's port becomes the ticket's redirect (path and query only). The
command gets 15 s, then its process tree is killed; stderr is discarded and its
output is never logged. The response adds `login`: `issued`, `notRunning`,
`spawnFailed`, `timeout`, `exitStatus` or `noLink`; on anything but `issued`
the ticket keeps the requested path. Views show `loginLinkCommand` to the owner;
agent views redact it like `command`.

Tickets are one-use, slug-bound, port-independent, valid for 60 seconds, and
stored only as hashes in memory (256 outstanding maximum). GET `/__cutex/auth`
consumes a ticket, sets a host-only HttpOnly SameSite=Lax Path=/ cookie, and
redirects to a safe relative path. Auth responses are no-store and no-referrer.
All other `/__cutex` paths return 404. Cookies use an instance-specific
`cutex_svc_<8 hex>` name, 32-byte random values and 12-hour sliding server
expiry. Restart invalidates them. The in-memory cookie table is bounded at 4096;
when full a new exchange fails closed and consumes the ticket.

Unauthenticated requests return 401 and `X-Cutex-Service-Auth: ticket-required`.
Management bearer tokens cannot authenticate the proxy and service cookies
cannot authenticate Management. Same-site/cross-site subrequests require
navigation fetch mode; otherwise 403. WebSocket upgrades additionally require
Origin host and port to match Host.

All `cutex_svc_*` cookies are stripped upstream. Forwarded host/proto/peer are
replaced with trusted values. Per-record `proxyHostHeader` defaults to `preserve`;
`upstream` rewrites it to `127.0.0.1:<service-port>`. With `upstream`, an `Origin`
that is exactly this proxy host's own origin (`http://<slug>.svc.localhost:<P>`, as
received) is rewritten to `http://127.0.0.1:<service-port>` too, so a service whose
CSRF / WebSocket gate admits only its own loopback origin accepts its own UI
(capability `services-login-link`). Any other Origin passes unchanged. One upstream socket serves
each browser connection. Content-Length/chunked uploads, SSE, keep-alive and
WebSocket 101 upgrades stream without whole-body buffering. Minimal response
framing prevents injecting a later error into an earlier response body; an early
upstream 413 cancels unfinished uploads.

Proxy connections release the Management slot and consume the separate proxy
budget. Every generated error carries `X-Cutex-Service-Error`:

| HTTP | Error |
| --- | --- |
| 404 | service_not_found |
| 410 | service_reclaimed (reason and time in body) |
| 503 | service_stopped |
| 502 | upstream_unreachable |
| 503 | proxy_busy |
| 403 | cross_site_blocked |
| 421 | host_changed |

## Lifetime

Stored records without `lifetime` read as `persistent`. New owner registrations
also default to persistent. New agent registrations default to `ephemeral`,
including external services. Re-registering an agent's own existing service
without a lifetime preserves its lifetime.

An ephemeral service is reclaimed when any of these conditions holds:

- Its TTL from `createdAt` has expired, even if it has leases or traffic.
- Its owner session is missing or archived, when owner reclamation is enabled.
  A session-store read failure defers this check; it is not treated as absence.
- There are no viewing leases or active proxy connections, and the idle period
  since the latest traffic, creation, daemon start or final lease release has
  elapsed. Expiring the final lease starts this period at its expiry time.

The supervisor runs reclamation every second and before initial startup.
The decision and registry removal share the registry file lock, preventing a
concurrent lifetime upgrade from being overwritten. Only a process tree owned
by this supervisor can be stopped. External processes are never killed; static
services only lose their registration. `cutex/services/changed` emits reason
`reclaimed`, including when tombstone retention is zero. Re-registering a slug
clears its tombstone and old viewing leases.

Tombstones are stored in `services/registry.json` and include `slug`,
`displayName`, `ownerCutexSessionId`, `lifetime`, `reason` (`idle`, `ttl` or
`owner_archived`) and `reclaimedAt`.

Ephemeral managed services have a finite automatic restart budget; the initial
launch is not a restart. Exhaustion produces `health: "crashLooped"` and no
automatic retry. Explicit owner `/start` or `/open` resets the budget. Persistent
services retain the existing exponential backoff. The exhausted state and
restart count are persisted with runtime state.

### Agent restart (`service_restart`, capability `services-agent-restart`)

`service_restart {slug}` lets the owning agent restart its managed service after
editing the code it runs. Ownership is the same as for `service_update` /
`service_unregister` (only the agent that owns the record; `not_service_owner`
otherwise; project services also permit their current Director). It needs no approval: command, cwd and env are unchanged, and the
`managed_service` grant that let the command run covers its restarts. It
refuses external and static services with 409 `not_managed` (cutex runs no
process for them) and a service the owner stopped with 409 `service_stopped`:
only the owner's `/start` or `/open` brings that back (the owner-start path also
clears `stopped`, which an agent must not override).

The Agent Bus serves agent tools and the supervisor runs in Management serve,
so the Bus only records a fresh `restartRequestedAt` on the registry record
(a revision bump, reason `updated`; not part of the launch fingerprint). On its
next pass (about a second) the supervisor sees the changed value and restarts the
process through the same path as `POST /v2/services/{slug}/restart`: kill the
running tree, wait up to 5 s for the port to free, start again with the restart
budget and backoff reset. If the owner stopped it (or it stopped being managed)
between the request and that pass, the request is dropped and noted in the log. A
value already present when the daemon starts is ignored. The tool answers right
after recording the request (`{ok, slug, restartRequested: true, service}`); the
agent checks `service_list` (`health`, `process.startedAt`) for the outcome.
Changing what the service runs is `service_update`, which restarts it by itself.

## Routes

| Method and path | Behavior |
| --- | --- |
| `POST /v2/services` | Existing register body plus `lifetime`, persistent-only `purpose`, `runtime: "static"`, `root` |
| `PATCH /v2/services/{slug}` | Owner can change retention plus persistent `purpose`, `groupName`, `starred`, `sortOrder`, `note` and `hidden` |
| `POST /v2/services/{slug}/archive` | Stops and hides the record without deleting its root/files; an agent cannot unregister it while archived |
| `POST /v2/services/{slug}/restore` | Restores the archived record to the main list; a managed service remains stopped until explicitly started |
| `GET /v2/services/{slug}` | Reclaimed slug returns 410 `service_reclaimed`, with `details.reason` and `details.reclaimedAt` |
| `GET /v2/services?tombstones=1` | Includes a `tombstones` array in addition to the normal list |
| `GET /v2/services?visibility=all` | Includes owner-hidden and owner-archived records; the default list omits both for old clients |
| `POST /v2/services/{slug}/lease` | Optional `{holder, clientLeaseId}` (`holder` at most 64, `clientLeaseId` 1–128 printable characters); returns 201 `{leaseId, expiresAt, ttlSeconds: 150}`. With `clientLeaseId` (capability `services-lease-idempotent`) a repeated open from the same credential renews and returns the live lease that key opened instead of creating another, so a retried request leaves no orphan lease; the key is scoped to the calling credential (client token or root) and expires with its lease. Without it every call opens a new lease |
| `POST /v2/services/{slug}/lease/{leaseId}` | Renews a live lease; 200 with the same shape; expired/unknown lease gives 404 `lease_not_found` |
| `DELETE /v2/services/{slug}/lease/{leaseId}` | Idempotent 200 `{released: bool}` |
| `PATCH /v2/services` | Updates settings below, validating the entire resulting settings object |
| `GET /v2/settings` | Also exposes the registry settings as `services` (capability `services-ephemeral-ttl`) |
| `PATCH /v2/settings` | `{services:{ephemeralIdleMinutes}}` updates the default; this nested setting is patched on its own |
| `GET /v2/services/{slug}/tile` | The home page tile: `{tile, fetchedAt, error, source}` (see Tiles below) |
| `GET /v2/services/{slug}/tile/image` | The tile's image (PNG, JPEG or WebP) |

There are at most 64 live leases per service. Leases are memory-only and do not
survive a daemon restart. Clients should renew every 60 seconds and acquire a
new lease after a 404. Leases require owner authorization just like all services
management routes; lease IDs alone are not credentials.

Management views add `lifetime`, persistent-only `purpose`, `idleTtlMinutes`, `effectiveIdleTtlMinutes`,
`maxIdleTtlMinutes`, retention `pinned`, owner organisation (`groupName`, `starred`,
`sortOrder`, `note`, `hidden`, `archived`, `archivedAt`), `link` (`cutex:service/<slug>/path`), `leases`,
`expiresAt`, `idleReclaimAt` and `restartsLeft`. Persistent and pinned services have null
expiry and idle deadlines. A lease or an active connection also makes the idle
deadline null. `restartsLeft` applies only to ephemeral managed services. Proxy
fields are `proxyHost`, `activeConnections` and `lastTrafficAt`; the supervisor
remains the sole writer of `state.json`, persisting traffic timestamps at most
once per minute. Agent tools include lifetime/link and preserve their existing
visibility rules; a peer's static `root` is redacted like its command/cwd. Owner organisation is
redacted from every agent view. Purpose is deliberately an open string in views so an older client
can put a future value under “uncategorised”; this host accepts `foreground` and `background` on writes.

An agent may suggest `purpose` only while registering a persistent service. An owner purpose edit wins
over later re-registers of the same slug. All owner organisation survives those re-registers and agents
cannot write it. Ephemeral services have no purpose; the desktop presents them as “temporary”. Turning
one persistent clears purpose, while the owner's “keep” action sends `lifetime: persistent` with a chosen
purpose (foreground by default). Turning a service ephemeral clears purpose. Hidden keeps running. Archived stops, hides and is excluded from automatic
TTL reclaim so it remains restorable; it never deletes the service's root or any file.

| Setting | Default | Accepted range |
| --- | --- | --- |
| `ephemeralIdleMinutes` | 30 | 0, or 5–1440 (`0` = until owner archive) |
| `ephemeralTtlHours` | 24 | 1–720 |
| `reclaimOnOwnerArchive` | true | boolean |
| `tombstoneDays` | 7 | 0–90 |
| `ephemeralMaxRestarts` | 2 | 0–10 |
| `proxyMaxConnections` | 192 | 16–1024 |
| `agentExternalDefaultLifetime` | ephemeral | ephemeral / persistent |
| `agentPersistentApproval` | ask | ask / off |

The external default is owner-configurable; with `agentPersistentApproval = ask`
an agent requesting the persistent default still goes through the lifetime
approval gate, with `off` it does not (see Agent approvals below). The effective lifetime
is pinned before that mutation. For private debug smoke only,
`CUTEX_SERVICES_DEBUG_IDLE_SECONDS=1..300` overrides the idle interval when
`CUTEX_TEST_PRIVATE_HOME` (or `HOME` in an isolated instance) is marked,
outside the real home, and contains the
active state directory. Release builds omit this override.

## Agent approvals and reveal

`service_register` accepts `lifetime`, persistent-only `purpose`, `idle_ttl_minutes`, `runtime: "static"`, `root` and `reveal`.
`service_update` accepts `lifetime` and `idle_ttl_minutes`; only the owning agent can edit or downgrade
its service. `service_update` raises the managed-execution approval only when
the effective runtime, command, cwd or env (after the env merge) differ from the
stored record; restating unchanged values, or changing only port, path, name or
lifetime, does not. The owner setting `agentPersistentApproval` (services settings,
`PATCH /v2/services`, capability `services-persistent-approval`) decides
whether requesting persistent lifetime (explicitly, or through a persistent
`agentExternalDefaultLifetime`) needs an owner approval:

- `ask` (default): an owner approval of kind `service_lifetime`, even if the
  agent already has unrestricted execution. Retry the same request after
  approval.
- `off`: the persistent lifetime is granted without an approval.

The owner can always set lifetime directly through the management route.

Every service approval (`managed_service`, `service_lifetime`, `static_root`)
holds the agent's call up to 120 s when its façade opted in (capability
`approval-wait`, `docs/agent-tool-security.md` §7.1): approved in that window,
the call registers / updates and returns the normal result; denied, it answers
`approval_denied`; otherwise `approval_required` as described here, and the
agent retries after the decision notice.

The managed-execution gate (`managed_service`) is separate and does not depend
on this setting. With `ask`, when a single request also needs a
managed-execution approval, one `service_lifetime` approval explicitly states
both grants and binds them to the exact payload digest. This prevents a
two-gate retry loop caused by consuming one approval before waiting on the
other. With `off`, such a request raises only the ordinary `managed_service`
approval (its digest still covers the whole payload, lifetime included).
Environment values are omitted from the approval card and still included in
its digest. Managed-only requests always use `managed_service`.

| `agentPersistentApproval` | Request | Approvals raised |
| --- | --- | --- |
| ask | persistent (external/static, or the persistent default) | `service_lifetime` |
| ask | managed outside the caller's envelope + persistent | one `service_lifetime` (both grants) |
| ask | managed inside the caller's envelope + persistent | `service_lifetime` |
| off | persistent (external/static, or the persistent default) | none |
| off | managed outside the caller's envelope + persistent | `managed_service` |
| off | managed inside the caller's envelope + persistent | none |
| either | managed outside the envelope, ephemeral | `managed_service` |
| ask | static root outside the caller's cwd / workspace + persistent | one `service_lifetime` (both grants) |
| either | static root outside the caller's cwd / workspace, otherwise | `static_root` |

Successful registration with `reveal: true` publishes
`cutex/services/revealRequested` on `cutex.host` with `slug`, `hostId`,
`ownerCutexSessionId`, `path` and `requestedAt`. Limits are one request per slug
per 60 seconds and ten per agent per hour. The result's `reveal` is `requested`,
`rateLimited` or `unavailable` (publishing failed); registration itself still
succeeds. The UI decides how to present a reveal request.

For owner-facing output use static content or a managed command. Vite commands
should use `--port {port} --strictPort`. Link the returned
`cutex:service/<slug>[/<path>]` URL. Its key encodes query delimiters, whitespace
and existing percent escapes so the desktop can decode it exactly once.
The Output line of `# Cutex` gives this guidance; the rest of the harness is
unchanged.

## Static runtime

`root` must be an absolute existing directory or `.html` file. A single-file
root serves its containing directory, with `/` mapped to that file. Owner roots
cannot be inside the cutex home. Agent roots (capability `static-root-approval`,
`services::static_files::check_root`), compared canonically:

| Agent root | Result |
| --- | --- |
| inside the caller's cwd or its project's `workspace_root` | allowed |
| inside the caller's own cwd when that cwd is a cutex-managed scratch workspace `<home>/agents-scratch/<id>` or `<home>/chats/<id>` | allowed (the only part of the cutex home that can be served) |
| any other existing directory / `.html` file outside the cutex home | owner approval: kind `static_root` (reason `static_root_outside_workspace`), or folded into the one `service_lifetime` approval of a persistent request; `details.staticRoot` = the canonical root, bound by the digest |
| inside the cutex home (otherwise), containing the cutex home, a filesystem root, the user home or a directory containing it | 403 `static_root_forbidden` |
| relative, missing, neither a directory nor an `.html` file | 400 |

The agent API validates the whole request (`ServiceStore::validate_register` /
`validate_update`, a dry run of the store mutation) before any approval gate, so
an approval is only raised for a request that would then succeed. Refusal
details name the root that was sent and list the caller's allowed roots (and
cwd / workspace entries ignored because they lie in the cutex home). A
re-register or update that keeps the root its record already stores asks
nothing again; a different root is a new request and a new approval.
Static services have no command,
cwd, environment or port; storage uses port zero, while `port`, `url` and
`localUrl` in views are null. Health follows root existence.

After proxy authentication, call
`services::static_files::serve(record, method, target, stream)`. It writes one
response and closes the connection. It permits GET/HEAD only, decodes URL
paths, refuses hidden/traversal components, backslash, NUL, control characters,
Windows alternate streams and malformed escapes, follows symlinks only within
the root, and serves `index.html` without directory listings. Canonical hidden
targets and the configured cutex home are also refused. Responses include
`X-Content-Type-Options: nosniff` and `Cache-Control: no-store`.

The implementation checks canonical paths before opening a file. It does not
claim protection against a local process concurrently replacing filesystem
objects between that check and open; OS-specific handle-relative traversal is
outside this change. Treat a registered tree as locally trusted content.

## Tiles (capability `service-tiles`)

A service can give the owner's home page a tile: a small read-only template the desktop draws in
its own style (cutex-desktop docs/research/home-page-20261019.md §2). Register / update
(`POST|PATCH /v2/services`, `service_register` / `service_update` as `tile` / `tile_path`) take:

- `tile`: a static template, validated on write (400 `invalid_tile`, the message names the field);
  `null` on update clears it. The service view shows it as `tile`.
- `tilePath`: a path inside the service that serves the template as JSON; `null` on update
  restores the default. Unset: `/.well-known/cutex-tile.json`, unless a static `tile` is set (then
  nothing is fetched). Static services never serve dot paths, so they need an explicit `tilePath`.

Template fields (all optional; unknown fields are dropped, also inside `progress` and `actions`):

| Field | Shape |
| --- | --- |
| `title` | 1–60 characters |
| `subtitle` | 1–120 characters |
| `lines` | at most 3 strings of 1–120 characters |
| `image` | a path inside the service (PNG / JPEG / WebP, served by `/tile/image`) |
| `progress` | `{value: 0..1, label?: 1–40 characters}` |
| `badge` | 1–12 characters |
| `actions` | at most 3 `{label: 1–20 characters, path}`; the desktop opens the path in the service view, it never calls it as an API |
| `refreshSeconds` | integer, clamped to 60–86400 (default cache 300 s) |
| `sizes` | sizes the tile supports, subset of `s`, `m`, `w`, `l` (unknown sizes dropped) |

Text is trimmed and may not contain control characters. A path starts with exactly one `/`, is at
most 512 bytes, has no whitespace, `\`, `#`, `.` / `..` segment or `/__cutex` prefix (also after
percent decoding); a query is kept. URLs, `javascript:`, `data:` and protocol-relative `//host`
values are refused.

`GET /v2/services/{slug}/tile` answers `{tile: {...}|null, fetchedAt, error: {code, message}|null,
source: "static"|"service"}`:

- static `tile` without `tilePath`: that template, `source: "static"`, `fetchedAt` = the record's
  `updatedAt`; no fetch.
- otherwise the daemon fetches the path the way the proxy reaches the service (loopback port with
  the proxy's `Host`, `X-Forwarded-Host: <slug>.svc.localhost`; a static service's root through the
  static file rules), within 2 s, at most 16 KiB of JSON, following at most 3 redirects and only
  those that stay inside the service (its proxy host or its own loopback port). It never starts a
  stopped managed service (`service_stopped`). The answer is cached per service for the template's
  `refreshSeconds` (300 s without one), a failure for 60 s; a record change refetches.
- a failed fetch returns the static `tile` (if one is set, `source: "static"`) or `null`, with
  `error.code` one of `service_stopped`, `unreachable`, `timeout`, `too_large`, `not_found`,
  `http_status`, `redirect_outside_service`, `too_many_redirects`, `invalid_response`,
  `invalid_json`, `invalid_tile`. Nothing at the default path is not an error (`tile: null`,
  `error: null`).

`GET /v2/services/{slug}/tile/image` fetches the current tile's `image` with the same rules (at most
512 KiB) and answers it with the type sniffed from its bytes (PNG, JPEG, WebP; SVG and anything else:
415 `tile_image_unsupported`), `X-Content-Type-Options: nosniff`, `Content-Security-Policy:
default-src 'none'; sandbox`, `Cache-Control: private, max-age=60`. No image: 404
`tile_image_not_found`; a failed fetch: 502 `tile_image_unavailable` (message starts with the
fetch error code). Access is that of the services routes: the owner; device scope `read` may use
both (they are `GET`s). Schema export `services`: `tile`, `tilePath`, `tileAnswer`.

## Schemas and rollback

Capabilities `services-proxy`, `services-lifetime`, `services-ephemeral-ttl` and `services-organize` advertise the contracts.
The single LF `services.json` exports views, settings, tombstones, leases and
view tickets. The methods schema includes `reclaimed` and `revealRequested`.

The first supervisor start migrates an old registry under its file lock, saves
its exact original bytes to `services/registry.json.pre-services-proxy`, and
writes `schemaVersion: 3` (v2 preserves retention `pinned`; v3 adds purpose and owner organisation) with persistent defaults for old records. Mutations
also perform this migration before writing. The backup is never overwritten;
subsequent starts do not migrate again.

Rollback: stop the daemon, restore `registry.json.pre-services-proxy` as
`registry.json`, then switch to the old binary. Registrations made after the
backup will be absent. Do not run old and new daemons against one home.

The r258 previous-build preflight refuses rollback while the registry contains
new retention intent (`ephemeralIdleMinutes: 0`, per-service TTL/pin) or any
persistent purpose / owner organisation state. The normal upgrade backup also
includes the complete `services/` directory; the preflight never strips fields.

Old cutex ignores unknown lifetime/root/settings/tombstone fields and drops them
on its next save (so does a cutex before `service-tiles` with `tile` / `tilePath`; the
desktop then shows the icon tile). However, `runtime: "static"` is an unknown enum variant to the
old reader: **one static record makes the entire registry fail to deserialize**.
Unknown `crashLooped` runtime snapshots are discarded by the old reader's
existing load-state fallback.

## Reproducing acceptance

Run `scripts/check-services-proxy.mjs` with the debug binary, optional 757f766
binary, and optional Management/Bus ports (defaults 26170/26160). The lifecycle
entry point runs the same combined smoke. The owner-approved daemon environment uses `HOME` pointing to marked
`~/.cutex-gpt-svcproxy-dev`, `CUTEX_HOME` pointing to its `.cutex` directory,
and `CUTEX_TEST_FORBID_REAL_HOME=1`. Only daemon/instance child processes omit
`CUTEX_TEST_PRIVATE_HOME`: r223 forbids combining it with `CUTEX_HOME`, while
its default instance rejects the allocated 26170/26160 ports. Rust tests retain
both `HOME` and `CUTEX_TEST_PRIVATE_HOME` in the marked test home. The pool is 26180–26199, child processes are
hidden, and only owned PIDs are stopped. The script refuses protected ports
and existing dev state. Archive previous state inside this private home before
another run. Generated credentials never appear in `result.json`. Visible
Chromium acceptance requires the owner's approval.
