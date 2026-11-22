# Job Service (optional component `job-service`)

A small Linux daemon that runs long commands for agents after their turn ends, keeps the job records
and output, and tells the agent when a job finishes. It also tracks jobs agents submit to a remote
scheduler (NCI Gadi PBS), which the desktop's **NCI remote jobs** module needs (not usable in the
public 0.1.x release yet).

**Who needs it:** people who want agents to start long-running jobs (the desktop **Jobs** page).
Everyone else can skip it. Linux hosts only.

## Install

From the desktop: open **Jobs** and click **Install** on the Job Service card. From a shell on the
host:

```sh
cutex component install job-service           # binary install (signed release archive)
cutex component install job-service --from-source ~/src/cutex   # source install (builds this crate)
cutex component status job-service
cutex component remove job-service            # stops it; job records and output are kept
```

cutex takes the archive `component-job-service-<version>-linux-x64.tar.gz` named in the signed release
manifest of the cutex release it runs, checks its SHA-256, unpacks it to
`<install base>/components/job-service/<version>/`, and runs it as the systemd user unit
`cutex-job-service.service`. New agents then get the `cutex_job` tools. Backend updates keep it in
step with cutex; a restart that would interrupt a running job waits until the job has finished
(status `restartPending`).

Its state, the private socket and its two credentials live in `~/.cutex/runtime/job-service/v1/`
(owner-only files; never shown or printed). NCI support reads the notification address cutex writes
to `~/.cutex/runtime/inbound-mail/notify-address-v1.json`; until inbound mail is configured, agents
are told that NCI mail is not set up.

## End follow-up (owner only)

`POST /v2/job-service/close-follow-up` accepts
`{jobId, expectedRevision, reason}` through the owner's authenticated Management API.
The capability is `humanOperations: closeFollowUp`; the private socket method is
`humanCloseFollowUp`. Agents have no corresponding MCP tool.

The root-only socket `humanQuery {jobId}` returns the full record without changing
state. The await reconciler checks its subscriber session before using it. A close
socket response also carries a per-call `followUpCommitted` boolean: true only for
the newly persisted disposition, false on replay or an existing terminal outcome.
The Management bridge removes this transient receipt flag and invokes the await
settlement hook only on a new commit. No completion message or turn is generated.

The reason must be nonempty, at most 2048 UTF-8 bytes, and contain no control characters.
An accepted disposition increments the revision and durably records terminal state
`unconfirmed`, `terminalReason: followUpEnded`, and
`followUpEnded: {expectedRevision, reason, atEpochSecs}`. Repeating the original revision
and exact reason replays the disposition, including after restart. Other stale requests
conflict. An existing terminal outcome remains unchanged, preserving its frozen completion facts.

This ends tracking only: it sends no process signal and does not cancel an external job.
The original local process remains owned until it exits naturally. Late process or
scheduler results do not reopen the record or start a completion turn. Unknown wire
states are read as terminal `unconfirmed`, without inventing an exit result.

Human list rows expose `processAlive` independently of tracking state. This remains true
while the service owns a live entry or the recorded PID and process birth still identify
a live local process. Idle-stop and maintenance checks must count these processes even
when the job is `unconfirmed`; ending follow-up is not permission to restart its service.
The separate `launchPending` flag preserves admission in progress until the process
identity is recorded (or launch fails). Recording that identity may advance the job
revision without reopening follow-up. A service restart clears the old admission flag.

## Build

`build.sh <platform> <version> <out-dir>` (CI runs it for every release; `<version>` must equal
`component.json` and `Cargo.toml`) builds the release binary from an LF export and writes
`component-job-service-<version>-<platform>.tar.gz` (the binary `cutex-job-service` at its root),
`SHA256SUMS-component-job-service-<platform>` and `BUILD-INFO-component-job-service-<platform>.json`.
Tests:
`cargo test --features test-harness` (some tests need an installed cute-codex CLI:
`CUTEX_JOB_TEST_LAUNCHER=/path/to/codex`).
