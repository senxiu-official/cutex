# Registry-driven iPad service gateway

Source update only: **do not install before the backend with
`services-remote-access-v1` ships**. The current installed gateway and certificate
kit remain unchanged by this branch. Hostname, mTLS leaf allowlists, loopback
bridge authentication, fixed service proxy, no network management page and
same-origin limitations remain as before.

All enabled pages share the configured browser origin, so a trusted page's scripts
can access other enabled pages and their same-origin storage. This grant is an
exposure switch, not isolation between services. Device mTLS remains required.

`gateway.py` grants access only when the freshly fetched service record contains
the boolean `remoteAccess: true`. Missing/false/invalid values fail closed.
`access.json` is never consulted, even if it exists or is recreated. Archived
services and portless workers remain excluded. Existing HTTP/Origin/SW/credential
stripping and internal proxy-cookie rate limits are retained.

Owner-local `remote_access.py` uses the original private `bridge.json` and
Management credential, without copying/logging it. It exposes no HTTP interface.
The fixed API wrapper allows only list/capabilities, per-service view-ticket,
and owner PATCH with `remoteAccess`/`ifRegistration`. The browser handler never
calls that PATCH path.

After backend deployment, a future authorized gateway update should:

1. Stop only the private iPad adapter before replacing its Python source. Keep
   Caddy/device certificates and other tunnels unchanged. Stop using the old
   file-writing `manage.py enable/disable` commands.
   At authorized deployment time, add the explicit lowercase DNS `host` to the
   private `bridge.json` (for example `services.example.test`); no default host
   exists and missing/invalid configuration refuses startup. Optional
   `host_label` controls the escaped page heading and defaults to that host.
   Retain existing secret/port/credential-path settings. Do not change the live
   configuration while only reviewing or testing this source branch.
2. Preview the migration with the existing private configuration:
   `python remote_access.py migrate --config <private-install>/bridge.json`.
   It refuses hosts without the capability; preview writes nothing.
3. After owner-authorized review, run the same command with `--apply`. It refuses
   a still-listening private adapter (no grant writes), then matches
   every old grant by slug + registrant + createdAt, skips stale/missing entries,
   and uses the backend's atomic registration precondition for each grant.
   A replacement created between list and PATCH cannot inherit the grant.
   An empty allowlist migrates zero permissions.
4. Each attempt/success is atomically journaled in the private
   `remote-access-migration.json`, including slug, registrant and creation time.
   Failures retain partial progress and the old file. A retry continues only
   unfinished entries and never PATCHes an already completed entry, even if the
   owner has since disabled it. An unconfirmed write (for example a reply lost
   after commit) is never replayed automatically; it appears in
   `skippedUnconfirmed` for owner review and can later be enabled explicitly.
   After completion, delete only the identical old `access.json` and start the
   updated adapter. A completed receipt only allows retirement, never re-grants.

No code path ORs old and new permissions. A changed file appearing after a
receipt is an error, not a new source of grants. Existing responses already in
flight may finish when the owner disables a service; new requests re-read the
registry before issuing/viewing a service ticket.

The local fallback CLI is now `remote_access.py list`, `enable <slug>` or
`disable <slug>`. Desktop uses the same owner PATCH and capability contract
in`docs/management/service-remote-access.md`. Do not expose these controls on
the iPad origin.

Run private loopback tests with Python3.12:
`python -m unittest -v test_gateway.py test_remote_access.py`.
They use fake Management credentials and temporary fixtures, never the live home.
