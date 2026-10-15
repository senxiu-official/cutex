# Host health

`GET /v2/health` is owner-only and returns Management contract version 2,
scan time and findings. `POST /v2/health/fix` requires the owner credential and
an `Idempotency-Key`; its JSON body accepts `findingIds` and `dryRun` (default
true). Inspect `planned` before sending the same selection with `dryRun:false`
and a new idempotency key. Changed observations return `409 finding_changed`.

`cutex human doctor` retains its local diagnostic output. `--check` scans;
`--fix` previews; `--fix --yes` applies safe fixes. The optional session selector
filters findings. The CLI does not start Management or Bus services.

`config.json` preserves `health: {"scan_on_start":false}`; default true starts
one background read-only scan in Management and logs warning/error codes.
Startup never fixes or sends owner notifications.

Runtime cleanup requires local ownership, at least one positive recorded PID,
every recorded PID verifiably dead, and no pending launch/launch claim. Living,
possibly reused, absent, zero, or uncertain PIDs are never cleared. Claude
cleanup also holds its controller lock. Runtime fields are cleared under the
cross-process session-store writer lock using the existing reconciliation
helper; no process is stopped and no record/file is deleted. Duplicate and
orphan identities are report-only if these conditions cannot be proved.

Missing-package findings use existing pinned-package evidence. Only an enabled
local persistent explicit-launch host agent with a v4 manifest, intact native
home, no runtime claim, and missing pinned files can be offered repair. Existing
review/apply checks validate the native thread and selected package again and
refuse changed evidence; other missing packages remain report-only.

Stuck Bus messages link to the existing owner list/discard interface. Queued
alarms without a durable message record produce `lost_alarms`; no past alarm is
replayed or migrated. Findings contain full IDs and no credentials or messages.
Every applied fix appends a `health_fixed` audit fact and a session runtime event.

On Unix, `profile_directory_writable` reports a `profiles` root or direct profile
subdirectory with group/other write bits set. Its detail includes the exact path,
observed octal mode, owner repair guidance in English and Chinese, and a
`repairCommand` argument array for `chmod 0700 -- <path>`. Review that directory
before applying the command yourself. These findings are always report-only
(`fix:null`); neither startup scanning nor `/v2/health/fix` changes permissions.
Absent directories are left absent; symlinks and nested directories are not
traversed. This check does not replace the launch-time auth custody validation.
