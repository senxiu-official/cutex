# Services: authenticated proxy, lifetime and static content

The Management v2 services routes require the owner credential (authorized
client/device tokens keep their existing owner mapping). Agent Bus service
tools use the authenticated agent's durable session and the existing project
visibility boundary. No caller identity is accepted from tool arguments.

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
otherwise). It needs no approval: command, cwd and env are unchanged, and the
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
| `POST /v2/services` | Existing register body plus `lifetime`, `runtime: "static"`, `root` |
| `PATCH /v2/services/{slug}` | Owner can change `lifetime` directly |
| `GET /v2/services/{slug}` | Reclaimed slug returns 410 `service_reclaimed`, with `details.reason` and `details.reclaimedAt` |
| `GET /v2/services?tombstones=1` | Includes a `tombstones` array in addition to the normal list |
| `POST /v2/services/{slug}/lease` | Optional `{holder, clientLeaseId}` (`holder` at most 64, `clientLeaseId` 1–128 printable characters); returns 201 `{leaseId, expiresAt, ttlSeconds: 150}`. With `clientLeaseId` (capability `services-lease-idempotent`) a repeated open from the same credential renews and returns the live lease that key opened instead of creating another, so a retried request leaves no orphan lease; the key is scoped to the calling credential (client token or root) and expires with its lease. Without it every call opens a new lease |
| `POST /v2/services/{slug}/lease/{leaseId}` | Renews a live lease; 200 with the same shape; expired/unknown lease gives 404 `lease_not_found` |
| `DELETE /v2/services/{slug}/lease/{leaseId}` | Idempotent 200 `{released: bool}` |
| `PATCH /v2/services` | Updates settings below, validating the entire resulting settings object |

There are at most 64 live leases per service. Leases are memory-only and do not
survive a daemon restart. Clients should renew every 60 seconds and acquire a
new lease after a 404. Leases require owner authorization just like all services
management routes; lease IDs alone are not credentials.

Management views add `lifetime`, `link` (`cutex:service/<slug>/path`), `leases`,
`expiresAt`, `idleReclaimAt` and `restartsLeft`. Persistent services have null
expiry and idle deadlines. A lease or an active connection also makes the idle
deadline null. `restartsLeft` applies only to ephemeral managed services. Proxy
fields are `proxyHost`, `activeConnections` and `lastTrafficAt`; the supervisor
remains the sole writer of `state.json`, persisting traffic timestamps at most
once per minute. Agent tools include lifetime/link and preserve their existing
visibility rules; a peer's static `root` is redacted like its command/cwd.

| Setting | Default | Accepted range |
| --- | --- | --- |
| `ephemeralIdleMinutes` | 30 | 5–1440 |
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

`service_register` accepts `lifetime`, `runtime: "static"`, `root` and `reveal`.
`service_update` accepts `lifetime`; only the owning agent can edit or downgrade
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

## Schemas and rollback

Capabilities `services-proxy` and `services-lifetime` advertise the contracts.
The single LF `services.json` exports views, settings, tombstones, leases and
view tickets. The methods schema includes `reclaimed` and `revealRequested`.

The first supervisor start migrates an old registry under its file lock, saves
its exact original bytes to `services/registry.json.pre-services-proxy`, and
writes `schemaVersion: 1` with persistent defaults for old records. Mutations
also perform this migration before writing. The backup is never overwritten;
subsequent starts do not migrate again.

Rollback: stop the daemon, restore `registry.json.pre-services-proxy` as
`registry.json`, then switch to the old binary. Registrations made after the
backup will be absent. Do not run old and new daemons against one home.

Old cutex ignores unknown lifetime/root/settings/tombstone fields and drops them
on its next save. However, `runtime: "static"` is an unknown enum variant to the
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
