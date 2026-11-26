# Host health

Health is a host-owner feature. Management accepts the root credential or a full
device credential belonging to the owner. It advertises `health-check` for the
report and `health-review-v1` for reviewed fixes.

1. `GET /v2/health` returns `contractVersion`, `scannedAt`, `findings`, and
   `claudeObservers`. Each finding has an opaque `id`, stable `code`,
   `severity`, optional `cutexSessionId`, `detail`, and a nullable `fix`.
2. `POST /v2/health/fix` with `Idempotency-Key` and
   `{"findingIds":["<id>"],"dryRun":true}` previews the chosen findings.
   The response contains `planned` and `skipped`. Multiple findings can
   describe the same repair. The preview lists each repair once and returns
   `previewToken` when there is something to apply.
3. After owner confirmation, POST to the same route with a new
   `Idempotency-Key` and
   `{"findingIds":["<planned.id>"],"dryRun":false,"previewToken":"<token>"}`.
   Send the exact set of IDs in `planned`. The token expires after ten minutes
   or a Management restart. Every new apply rechecks current evidence. A replay
   of a retained result uses the same idempotency key and request body.

Apply reports `applied`, `skipped` and refreshed `findings`. On HTTP 409,
`error.details` can include `applied`, `skipped`, and `failedFindingId`.
A missing, expired or mismatched preview returns `preview_required`. A changed
finding returns `finding_changed`. Refresh and review again after either error.
A whole-host preview with more than 1,000 planned fixes returns
`preview_limit`. Choose up to 1,000 finding IDs and preview that selection.
On an interrupted response or `request_in_progress`, inspect health and
reconcile the outcome before starting a new apply with a different key.

`cutex human doctor --check` scans. `--fix` previews and `--fix --yes`
explicitly applies, optionally scoped to a session or `--finding-id`. The
offline CLI uses its existing local confirmation flag. Configuration
`health: {"scan_on_start":false}` disables the default read-only startup scan.

## Runtime evidence

- A local stale runtime binding needs at least one positive PID, all recorded
  PIDs absent, and no pending launch markers. If a PID is live, reused or
  unqueryable, leave the binding in place. The Claude previous-boot case
  instead requires the exact recorded adapter descriptor and binding evidence.
  Apply rechecks the occurrence under the session writer lock. Claude also
  checks endpoint reachability, holds its controller lock and checks the Linux
  adapter scope. The fix clears session runtime fields without signalling a
  process or deleting files.
- `unverified_launch_claim` reports a launch marker with missing or absent
  recorded PIDs. A launch may own a publication lease before a child PID is
  written. The stock lifecycle reconciles claims using the durable receipt and
  publication lease under its launch fences. Health keeps those claims
  report-only. Duplicate and orphan runtime IDs also require safe runtime
  evidence before they can be cleared.
- Managed runtime package directories use the existing Runtime Versions cleanup
  flow. `runtime-prune-v1` provides GET and POST `/v2/runtimes/prune`.
  GET reports version IDs, logical bytes, eligibility and refusal reasons.
  After confirmation POST accepts only selected eligible IDs. Apply rechecks
  pins, sessions, rollback and process evidence under the existing fences.
  See [runtime cleanup](management/runtime-prune.md). Session runtime data is
  protected here. Supported disposable agent homes use the separate Storage
  cleanup inventory and its preview/apply flow.
- A missing pinned runtime package can offer package repair. The existing
  package review and apply validate the Native thread, selected package and
  current session evidence. Other missing package findings are report-only.
- Stuck Bus messages use the existing owner message action. Lost alarms are
  report-only. Eligible Unix profile directories can offer a guarded mode
  repair to 0700. Non-owned directories and unverified paths remain report-only.

Successful health fixes append an audit fact. Session runtime fixes also
publish an offline event when the session still has no successor runtime.

## Desktop Settings handoff

Add **安全体检 / Health check** to host Settings. Use the selected host and the
existing Electron main host client. Expose only fixed read/preview/apply
operations through preload. Gate actions on owner/full access and
`health-review-v1`. Where an older host advertises `health-check` alone,
show the report in read-only mode.

Scan on entry and on explicit refresh. Show host, scan time, empty state and
localized finding codes. Link `cutexSessionId` to the agent. Allow selection
only for actionable rows. Present the returned plan in a confirmation dialog
and treat `planned` as the exact confirmation scope. Disable duplicate apply
while a request is in flight. Keep the host and preview paired and clear the
preview when either the selection or host changes. After apply show results and
refresh. On partial failure, show the applied IDs before refreshing.

Link to the selected host's existing Runtime Versions cleanup and Storage
cleanup surfaces if their capabilities are available. These flows own their
own previews and confirmations. Keep package repair, runtime record cleanup
and runtime directory deletion distinct in the UI because they change different
data and use different evidence.
