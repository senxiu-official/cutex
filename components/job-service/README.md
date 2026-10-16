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

## Build

`build.sh <platform> <version> <out-dir>` (CI runs it for every release; `<version>` must equal
`component.json` and `Cargo.toml`) builds the release binary from an LF export and writes
`component-job-service-<version>-<platform>.tar.gz` (the binary `cutex-job-service` at its root),
`SHA256SUMS-component-job-service-<platform>` and `BUILD-INFO-component-job-service-<platform>.json`.
Tests:
`cargo test --features test-harness` (some tests need an installed cute-codex CLI:
`CUTEX_JOB_TEST_LAUNCHER=/path/to/codex`).
