# Backend upgrade (capability `backend-upgrade`)

How a cutex installed from a signed release updates itself: `cutex upgrade …` on the host,
or the desktop's backend-update page (Settings → Backend). Code: `src/upgrade/`,
`src/management/v2/upgrade_api.rs`; schema export `upgrade`
(`src/management/v2/schema/export/upgrade.json`).

**Only managed installs are changed.** Everything that changes something requires
a *managed install* (`<base>/install.json`, written only by
`cutex service install`) that the running executable belongs to
(`<base>/releases/<r>/cutex`, `<base>/current/cutex`, `<base>/bin/cutex.exe`,
or `CUTEX_INSTALL_BASE`). A hand-deployed daemon is `managed: false`; every
mutation answers `409 upgrade_blocked {reason: "unmanaged_install"}` and the
scheduler never runs. An unmanaged daemon reads no layout at all (status and
settings are built in memory; there is no stand-in base such as a temp dir,
which another local user could seed with an `install.json`).

**Release file records.** A signed manifest only names each platform
archive's sha256. Right after unpacking a verified archive, the stager writes
`upgrade/release-files/<release>.json` (private): the sha256 of the unpacked
`cutex` / `cutex-mcp`, the manifest's sha256 and the archive's sha256
(`cutex service install` records the release it installs the same way).
Before any release executable is started — the preflight probes, the apply /
rollback runner, and the release a rollback switches to — the executable must
match its record, the record must match the release dir's `manifest.json`,
and for a staged release that manifest's signature must verify and name the
recorded archive. Otherwise: preflight check `integrity` fails, or the run
fails with `release_unverified`. A release dir without a record (staged by an
older binary) is re-checked against the signed archive by
`cutex upgrade stage <release>`.

## 1. Install layout and services

```
<base>/                               Linux: $XDG_DATA_HOME/cutex or ~/.local/share/cutex
  install.json                        Windows: %LOCALAPPDATA%\cutex   (override: CUTEX_INSTALL_BASE)
  releases/<release>/cutex[.exe], cutex-mcp[.exe], manifest.json(+.minisig), manifests/default.json
  natives/<version>/codex-app-server, codex-code-mode-host, codex, schema json
  current -> releases/<release>       Linux (units run current/cutex)
  bin/cutex.exe, bin/cutex-mcp.exe    Windows (rename-swapped copies; the old ones kept as *.old-<release>)
  run/                                Windows: supervisor.json, supervisor-control.json, supervisor.lock, cutex-supervisor.xml
  upgrade/settings.json, state.json, history.jsonl, upgrade.lock, cache/, release-files/<release>.json, runs/<runId>/{run.json, runner.log, backup/, displaced/}
```

`cutex service install [--base DIR] [--unit-dir DIR] [--channel release|main] [--no-start] [--dry-run] [--force]`
copies the running `cutex` + `cutex-mcp` into `releases/<label>/` (label =
build release, else `dev-<commit7>`), points the services at it and writes
`install.json`. `cutex service uninstall [--dry-run]` removes units / task only
(never data; `install.json` is renamed). `cutex service status` prints the
record and unit / supervisor state.

- **Linux**: systemd user units `cutex-agent-bus.service`,
  `cutex-management-api.service` (marker comment first line; a unit without
  the marker is refused unless `--force`), `KillMode=process` on both units so
  the agent runtimes either daemon started (Codex app-servers, Claude adapters
  with Claude Code and their tools, which live in that unit's cgroup) survive a
  daemon restart, `Environment=CUTEX_SERVICE_MANAGER=systemd`. Units written
  before r248 had it on the Management API only, so stopping the bus killed the
  runtimes it had started: the upgrade runner's stop and `cutex human service
  stop|restart` of the bus first add `KillMode=process` (+ `TimeoutStopSec=30`)
  to a bus unit that carries the marker and sets no `KillMode`, then
  `systemctl --user daemon-reload` (`upgrade::service::keep_bus_children`), so
  that very stop already keeps them; `cutex service install` writes the new text.
  With `--unit-dir` the units are only written (no `systemctl` at all — this
  is what tests use). A linger hint is printed when `Linger=no`. Units of an
  independent instance carry `Environment=CUTEX_HOME=…`.
- Unit / task names are recorded in `install.json` (`busUnit`,
  `managementUnit`, `taskName`); the runner, `status` and `uninstall` use the
  recorded names. Debug builds only: `CUTEX_SERVICE_TEST_NAME=<label>`
  installs as `cutex-<label>-agent-bus.service` / `cutex-<label>-management-api.service`
  / `\Cutex-<label>\cutex-supervisor` (end-to-end drills on a developer
  machine never touch the real names). The task principal is the user's SID
  (`whoami /user`).
- **Windows**: a per-user Task Scheduler task `\Cutex\cutex-supervisor`
  (LogonTrigger for the user, InteractiveToken, LeastPrivilege, no time
  limit, restart on failure) running `bin\cutex.exe service run`: a small
  supervisor inside cutex that starts / restarts the bus and management (with
  `CUTEX_SERVICE_MANAGER=cutex-supervisor`), detaches its console, is
  controlled through `run/supervisor-control.json` (`desired: running|stopped|exit`,
  `seq`) and reports `run/supervisor.json` (children, PIDs, creation
  times, `ackSeq`). Stopping kills only the two daemon PIDs, so native
  children survive (reload mode). A restarted supervisor adopts running
  daemons by PID + creation time. The task is refused on a host whose
  daemons are already run by another supervisor (a `runtime-host` endpoint). The desktop may start the task
  (`schtasks /Run /TN \Cutex\cutex-supervisor`) when management is down.
  `cutex human service` (TUI services page) on Windows uses, in order: a
  published runtime-host, an explicit `windows_service_controller`, then the
  supervisor of the managed install (Start = `schtasks /Run` when no
  supervisor runs, then `desired: running`; Stop / Restart act on both
  daemons). On Linux it controls the unit names recorded in `install.json`.
- **Notifications** (cutex 0.1.1+): neither manager starts a separate
  `cutex notify run`. `management serve` of a managed install
  (`CUTEX_INSTALL_BASE` / the executable's layout has `install.json`) embeds
  the notification worker: the state collector behind
  `/v2/notifications/states` and the webhook outbox. It takes the same worker
  lock as `cutex notify run` (`~/.cutex/notifications/outbound.lock`),
  retrying every 30 s, so a host that also runs `cutex notify run` delivers
  each notification once (whoever holds the lock delivers).
  `CUTEX_NOTIFY_WORKER=embedded|off` forces it on or off for any
  `management serve`. Hand-deployed hosts (no `install.json`) keep their own
  `cutex notify run` service. An existing managed install gets the worker
  with the next backend update (same units / task, new binary).
  `cutex service status` reports `notificationWorker` (heartbeat, webhook
  enabled, delivery counts). The webhook is sent with the linked HTTP client
  (no `curl` needed; no redirects; env proxies honoured).

`/v2/host` gains `serviceManager: {kind: systemd|cutex-supervisor|runtime-host|none, unit, managedInstall}`
and `runtimes` (owner only, else null): `{native: {selected, latest, inUse: [{release, version, agents, selected, missing}]}, claude: {selected, inUse}, unpinnedAgents}`.
`cutexBuild.channel` is `release` or `main` (`stable` → `release`, `dev` → `main`).

## 2. Release source and signatures

```
<source>/channel-<channel>/channel.json(+.minisig)  {schema: "cutex/channel/v1", channel, sequence, release, tag?, manifestSha256, publishedAt?}
<source>/cutex-<release>/manifest.json(+.minisig)   {schema: "cutex/release-manifest/v1", channel, sequence, publishedAt, notes{en,zh-CN},
                                                     cutex{release, commit, capabilities{added,removed}, migrations[{id,class,since,notes,newStores}],
                                                           upgradeFloor?, artifacts{linux-x64|windows-x64: {tag?, name, sha256, size}}},
                                                     native?{version, commit, nativePatchCommit?, artifacts{…}}, components?{<name>: {version, artifacts{…}}},
                                                     desktop{minCommit?, branch?}, mode: reload|full, packageRunRecommended}
<source>/<tag>/<name>.tar.gz                         cutex archive: cutex[.exe] + cutex-mcp[.exe] at the root
                                                     (+ LICENSE, NOTICE, THIRD_PARTY_NOTICES.md)
```

The stager unpacks every regular file of an archive and requires only the
executables it names (cutex: `cutex` + `cutex-mcp`; native: app server, code-mode
host, schema), so files next to them — the license files, `cute-codex-build.json`
— are tolerated by every release since 0.1.0 and land in `releases/<release>/` /
`natives/<version>/`. Never ship a file named `manifest.json`, `manifest.json.minisig`
or `release-receipt.json` (the stager writes those) or `receipt.json` in a component.

Default source `https://github.com/senxiu-official/cutex/releases/download`
(GitHub release assets); `cutex upgrade config --source` accepts an https URL,
a loopback `http://127.0.0.1…` URL or an absolute directory (tests and mirrors
use these; tests never reach GitHub). Archives are `.tar.gz` (flate2 + an own
ustar/pax reader — no new crate; RFC's `tar.zst` / `zip` would need new
dependencies). Paths in archives are sanitized; sha256 + size are checked
before unpacking.

**Signatures**: minisign (Ed25519; legacy `Ed` and prehashed `ED` with
BLAKE2b-512; trusted comment verified) implemented on `ring` (already a
dependency) plus an own BLAKE2b. Public keys are compiled in:
`src/upgrade/keys.rs` `EMBEDDED_PUBLIC_KEYS` (the release key and an
offline spare for rotation; the passphrase-protected secret keys never leave
the maintainers' offline storage), or `CUTEX_RELEASE_PUBKEYS` (`;`-separated)
at build time, for a fork that signs its own releases. Signing a release: `minisign -S -s cutex-release.key -m manifest.json -t "cutex <release>"`
and the same for `channel.json`. A build with no key at all (a fork that
removed them) fails closed on check / stage (`503 no_trusted_keys`) and
reports an empty `trustedKeys`. Debug builds only also
read `CUTEX_UPGRADE_TEST_PUBKEY` at run time; unit tests sign with a
deterministic in-test key (`minisign::test_key`) and an independently
generated vector.

## 3. Classification (RFC §6)

Migrations of the target manifest with `installed < since <= target`; the
class is the maximum (`reload-safe < rollback-safe < one-way`). Mode is
`full` when any migration is not reload-safe, when the release brings a
native version this host does not select, or when the manifest says `full`;
otherwise `reload`. `upgradeFloor` above the installed release →
`409 upgrade_blocked {reason: below_floor}`. A one-way apply needs
`confirmOneWay = <release>` (else `409 one_way_unconfirmed {migrations}`)
and is never rolled back automatically.

## 4. Apply run

`check` → `stage` (download, verify, unpack into `releases/<r>/` and
`natives/<v>/`, preflight: `--version` must show release + commit,
`upgrade selftest --json` of the NEW binary must answer `{"ok": true}`
(read-only checks of `config.json`, `accounts.json`, the session store),
native files, disk space, rollback target present) → `apply`.
`upgrade selftest`, `upgrade run`, `upgrade status`, `upgrade history` and
`service status` skip the one-time startup migrations (`fresh-agent-permissions`,
the legacy `~/.codez-cli` move), so a staged binary never migrates the live
home before it is activated; the new daemon migrates when it starts.

`apply` validates everything first (nothing changes on a refusal), records
the run (`queued`) and starts the TARGET release's binary detached as
`cutex upgrade run <runId>` (Linux with systemd: `systemd-run --user
--unit=cutex-upgrade-<runId>`, otherwise a new session / detached process),
which holds `upgrade.lock` and drives:

| phase | what happens |
|---|---|
| `waiting-idle` | polls `/v2/sessions?includeHidden=true` (+ an active runtime-package run) every 5 s; `run.waitingFor` lists what is busy (`activeTurn`, `threadActive`, `packageRun`). **Never interrupts a turn.** Not idle within `maxWaitSecs` → `deferred` (nothing changed). Cancel possible. |
| `quiescing` | only `full` + `packageRun: during`: online runtimes go offline one by one, re-checking idleness before each; a turn that starts brings them back and waiting resumes |
| `stopping` | services stop (systemd units / supervisor `stopped`; native processes stay) |
| `backing-up` | `runs/<id>/backup/home/…` + `home-manifest.json`: the daemon's state files / dirs, SQLite via `VACUUM INTO`, others sha256-verified |
| `swapping` | Linux `current` link / Windows rename swap; `install.json` `previousRelease`; full mode: `releases/<r>/manifests/default.json` (v4 runtime bundle with the new native / facade) selected for NEW agents (`runtime/light/deployment.json`), native registered |
| `starting`, `waiting-ready` | services start; `/v2/host` answers |
| `package-run`, `restoring-online` | only `during`: `POST /v2/runtime-packages/runs {all:true}` while offline, then the offlined runtimes back online |
| `verifying` | `cutexBuild.release` / `commit` match the manifest; no capability lost except `capabilities.removed`; every `capabilities.added` present; no NEW error-level `/v2/health` code; runtimes online before are online again (waits 60 s + 6 s per runtime, 180 s – 15 min; real deploys took 78 – 88 s for 20 – 22 runtimes). **Commit point.** |
| `committed` | `state.installed` / `previous` updated, staged / available cleared, older backups pruned (3 kept). `packageRun: after` (default, RFC §10.4): a follow-up moves idle outdated agents in batches of `packageRunBatch` (default 5) every minute until none is left (`followUp`) |

Failure before the commit point → `rolling-back`: revert a package run of
this upgrade, the old binary's rollback preparation (hazards → needs-owner),
stop, switch back, restore the selection files from the backup (full mode),
move the interval's `newStores` aside into `runs/<id>/displaced/` (never
deleted), start, wait ready → `rolled-back`. Session data are never
restored. Any failing rollback step, any failure of a one-way upgrade after
the swap, or a lost runner → `needs-owner` with `needsOwner.commands`.

## 5. Rollback (owner)

`cutex upgrade rollback [--to R] [--dry-run]` / `POST /v2/upgrade/rollback`:
target = `install.json` `previousRelease` (or `to`, any installed release
that is not newer: a target whose manifest sequence is higher than the
installed one is `409 no_rollback_target {reason: newer_than_installed}`;
apply it instead). Refused across a one-way migration (`409 upgrade_blocked
{reason: one_way}`). After a committed rollback `from → to`,
`previousRelease` becomes `to`'s own predecessor (the `from` of the committed
apply that installed `to`, if still installed and not newer) or is cleared,
so a second default rollback never goes forward to `from`.
Run by the running (newer) binary: wait-idle, stop, backup, switch, restore
the selection files saved by the apply that went `to → from` (if its backup
is still there; then mode `full`), move new stores aside, rewrite the records the
target release cannot parse or would read with more rights than they carry
(`src/upgrade/older_stores.rs`; each changed record copied to `displaced/older-stores/` first;
the same step runs when a failed apply rolls back), start, check `cutexBuild.release` →
`committed`. Each rewrite runs only when the target is older than the release that introduced
its records, or is not a plain `N.N.N` label (dev and `rN` labels: all run):

| since | record | rewrite for an older target |
|---|---|---|
| 0.1.4 | approvals of kind `static_root` | removed (asked again) |
| 0.1.4 | pending Claude `initial_permission_mode` | cleared (Profile default mode) |
| 0.1.5 | client / device token with scope `chat` / `read` | held: `tokenSha256` = `rollbackHeld.v1:<scope>:<hash>`, so an older release (no scopes) refuses the token instead of granting full rights; 0.1.5+ read it back with its scope (no re-pairing) |
| 0.1.5 | pending cross-host wake (`cutex.peer.tell.v2`) not yet handed to a runtime | turned into the v1 cross-host note (passive, digest recomputed); an older release would never deliver it and keep it as an eligible next step |
| 0.1.5 | unacknowledged `newOwnerDevices` in `users-v1.json` | copied aside only (an older release drops them) | The preview lists `migrations`,
`movedAside`, `selectionBackup` and `sessionDataRestored: false`.

## 6. CLI

```
cutex upgrade status [--json]
cutex upgrade check
cutex upgrade stage [RELEASE]
cutex upgrade apply [RELEASE] [--mode reload|full] [--max-wait SECS] [--confirm-one-way RELEASE]
                    [--package-run after|during|none] [--no-follow]
cutex upgrade cancel
cutex upgrade rollback [--to RELEASE] [--dry-run] [--max-wait SECS] [--no-follow]
cutex upgrade history [--limit N] [--json]
cutex upgrade config [--channel release|main] [--source URL|DIR] [--clear-source] [--check-interval-hours N]
                     [--auto-stage BOOL] [--auto-apply BOOL] [--package-run …] [--package-run-batch N] [--max-wait SECS]
cutex upgrade selftest [--json]          (read-only; preflight runs it with the new binary)
cutex service install|uninstall|status   (see §1)
```

`apply` / `rollback` follow the run's timeline until it is terminal (exit 1
unless `committed`).

## 7. HTTP API (owner only; access.rs `/v2/upgrade`)

All bodies JSON; unknown fields → `400 invalid_request`. Errors use the v2
error envelope (`error.code`, `error.details`).

| route | answer |
|---|---|
| `GET /v2/upgrade` | `200` status: `{managed, base, serviceManager, build, installed, previousRelease, installedReleases, lastCheckAt, lastCheckError, available, staged, staging?, run, followUp, settings, trustedKeys}` (`$defs/upgradeStatus`); `installed` = `{release, sequence, commit, manifestSha256, notes, publishedAt}` (`notes` / `publishedAt` from the installed release's `releases/<release>/manifest.json`, capability `upgrade-installed-notes`; null when there is none) |
| `POST /v2/upgrade/check` | `200` status after reading the channel |
| `POST /v2/upgrade/stage` `{release?}` | `202 {staging: {release, state: "running"}}`; progress / failure in status `staging` (`state: failed`, `error`) and events |
| `POST /v2/upgrade/apply` `{release?, mode?, maxWaitSecs?, confirmOneWay?, packageRun?}` | `202 {run}` |
| `POST /v2/upgrade/cancel` | `200 {run}` (`cancelRequested`; a queued run without a runner is cancelled at once) |
| `POST /v2/upgrade/rollback` `{to?, maxWaitSecs?, dryRun?}` | dry run `200 {preview}`; else `202 {run, preview}` |
| `GET /v2/upgrade/runs?limit=` | `200 {runs}` (current non-terminal run first, then history newest first; limit ≤ 200, default 20) |
| `GET /v2/upgrade/runs/{runId}` | `200 {run}` / `404 run_not_found` |
| `GET /v2/upgrade/settings` | `200 {settings}` |
| `PATCH /v2/upgrade/settings` `{channel?, checkIntervalHours?, autoStage?, autoApply?, packageRun?, packageRunBatch?, maxWaitSecs?}` | `200 {settings}`; `source` → `400 source_not_editable` (host CLI only) |

Error codes: `upgrade_blocked` (409, `details.reason`: `unmanaged_install`,
`run_in_progress` (+`runId`), `stage_in_progress`, `preflight_failed`,
`below_floor`, `one_way`), `one_way_unconfirmed` (409), `not_staged` (409),
`nothing_available` (409), `downgrade_refused` (409), `no_rollback_target`
(409), `no_run` (409), `not_cancellable` (409), `release_unverified` (409), `invalid_request` (400),
`source_not_editable` (400), `run_not_found` (404), `no_trusted_keys` (503),
`signature_invalid` (502), `upgrade_unavailable` (503, other failures).

Run object (`$defs/upgradeRun`): `runId` (`upg-<12 hex>`), `kind`
(`apply|rollback`), `fromRelease`, `toRelease`, `mode`, `class`, `phase`,
`createdAt`, `finishedAt`, `maxWaitSecs`, `packageRun`, `confirmOneWay`,
`requestedBy`, `waitingFor[{reason, cutexSessionId, name, detail}]`,
`cancelRequested`, `timeline[{phase, at, detail?}]`, `downtimeMs`,
`backupDir`, `packageRunId`, `error{code, message}`,
`needsOwner{reason, commands[]}`, `runnerPid`, `displaced[]`, and
(capability `upgrade-run-native`, recorded when the run is prepared, null on
older records) `nativeVersion` (the cute-codex native the target release
carries: apply = the staged manifest's `native.version`, rollback = the
target release's installed manifest), `previousNativeVersion` (apply: the
native selected for new agents then; rollback: the running release's) and
`runtimeChange`: `native` (full mode, a different native: a new native
runtime for new agents), `package` (full mode, same native: only the runtime
package for new agents changes), `none` (reload mode: selection untouched).
Terminal phases: `committed`, `rolled-back`, `needs-owner`, `deferred`,
`cancelled`, `failed`.

**Event** `cutex/upgrade/updated` (host event, `$defs/upgradeEvent`):
`{reason: startup|state, hostId, run: {runId, kind, phase, fromRelease, toRelease, servicesDown} | null, available, staged, staging}`;
published by a management watcher of `upgrade/state.json` (every 2 s), so
also the runner's writes and, after the restart, the run's state as seen by
the new daemon (`reason: startup`). While `servicesDown` the desktop should
expect the connection to drop and reconnect; after `committed` it re-reads
`/v2/capabilities` (new build) and `GET /v2/upgrade`.

**Scheduler** (managed installs with a trusted key only): every
`checkIntervalHours` (default 6; 0 = off) check; `autoStage` (default true)
stages a new release; `autoApply` (default false) applies a staged
non-one-way release (still waiting for idle agents). Neither touches a
release that a committed owner rollback left (until the owner applies it by
hand), nor one whose apply rolled back, went to `needs-owner` or failed for a
non-transient reason. Transient failures where nothing changed
(`management_unreachable`, `runner_lost`, `runner_start_failed`) are retried
at the next due check, at most 3 failures per release.

**Optional components** (cutex 0.1.1+: the Claude runtime, the Job Service) are staged with a
release and selected during the swap; a rollback selects the previous versions, and a component
failure never fails or rolls back the cutex update. See [components/README.md](../components/README.md).

## 8. Not done yet

- Rollback preparation hooks per migration (the old binary's prep step): the
  runner treats only one-way migrations as hazards.
- Busy reasons `askSubmitting` / `workflowRunning` (only turns, active
  threads and package runs are waited for).
- On Windows the logon start of the supervisor task and the absence of a
  console window under Task Scheduler were not covered by the end-to-end
  drill; busy waiting / `deferred` was not drilled on either platform.
