# r219 removals: legacy Task Service, Release rotation, Director seat copy

Status: implementation plan + record for branch `feat/r219-cleanup` (base `a567c27` = r218).
Owner decisions (2026-10-09):

- Delete the legacy **Task Service** (task actions, task delivery, watchdog, task reports, owner task
  reads, human task recovery, the `cutex_task_service*` tools).
- Delete **Release rotation** (`/api/rotation/v1/release`, `/v2/release-rotation*`, the release
  template store, `cutex agent release-rotation`, `cutex management release-rotation`).
- **Director = Agent Management `ProjectAuthority` only.** The per-project Director seat copy in
  the Task Service seat store goes away, together with the legacy global `cutex-director` /
  `cutex-release` seats and the `cutex management seat` administration surface.
- Remove the **`director_rotate`** agent tool / `DirectorRotate` operation. A future
  owner-initiated rotation is separate work.
- **No read-compat for the previous release.** r219 performs a one-time migration on load; the
  rollback path is "restore the pre-deploy backup" (see §8).

Asks / delegations, Job Service, Agent Management project authority (`create_project`,
`assign_director`, `clear_director`) and the agent bus messaging path are not changed beyond
removing the seat reads / writes and task branches listed below.

## 1. Removal map (approximate line counts at `a567c27`)

| Area | What goes | ~Lines |
|---|---|---|
| `src/task_service/` | whole module (provider v2 SQLite, watchdog, task detail, owner reads, schemas) | 18.2k |
| `src/task_delivery/` | whole module (provider adapter, pilot attempt fence, worker actions) | 5.2k |
| `src/rotation/` | whole module (release rotation provider, template store) | 3.0k |
| `src/seat/` | whole module (seat occupancy store, project Director seat transfers) | 1.8k |
| `src/role_revision/` | Role-Seat v1 operations / repository; shared newtypes (`CutexSessionId`, `Sha256`, `Rfc3339`, `ReceiptId`, …) stay | ~2–3k of 9.6k |
| `src/agent_bus/server.rs` + `server/task_action_store.rs` | `TaskWorkerActionHost`, completion drain, watchdog thread, all `/api/task/*` routes, `/api/rotation/v1/release`, poll completion fence, task tests | ~9.6k |
| `src/agent_bus/{model,queue,client,identity,tool_policy,mcp_management,mcp}.rs` | task metadata / action types, task enqueue fns, task + rotation client fns, task principal, `director_rotate` tool + rule, seat-based roles | ~1.3k |
| `src/app_server/{bus_bridge,bus_bridge_external,bus_bridge_task_display,activity_bridge,commands,external_recovery}.rs` | task delivery (legacy + ExternalInput paths), A4 task facts, pre-activation notices, task views, task activity items, seat locks | ~3.3k |
| `src/management/**` | `/v2/task-service/*`, `/v2/release-rotation*`, `/v2/tasks/{id}`, `/v2/sessions/{id}/tasks`, owner task reads on `/v2/projects/{id}/tasks`, task reports, task integration events, task turn bindings, seat admin credential | ~2.5k |
| `src/agent_management/**` | seat two-phase writes, `RepairDirectorSeat` execution, `DirectorRotate` execution, `ProjectTaskInspector`, `TaskServiceProvider` parameters | ~2k |
| `src/cli/args.rs`, `src/cli_app/**` | `agent task-action`, `agent release-rotation`, `agent manage director-rotate`, `management seat`, `management release-rotation`, `human tasks`, `--cancel-tasks`, TUI Tasks panel + task reports + Task Watchdog settings, `cli_app/rotation.rs`, dead new-thread launch tail | ~5k |
| config / notify | `CodezConfig.task_watchdog`, `owner_task_read_credentials`, `task_sort`; webhook events `task.review_ready` / `task.closed`; notification levels `task.reviewReady`, `task.closed`, `system.preActivationSkipped` | ~0.2k |
| `skills/cutex-rotate-director/` | whole skill | 0.1k |

The final numbers are recorded in §9 after implementation.

## 2. Keep-list and how each item is preserved

| Item | How it is preserved |
|---|---|
| Job Service completion token `runtime/task-service/job-service-completion.token` | Path re-anchored explicitly as `<runtime>/task-service/job-service-completion.token` (it used to be derived from the task-worker-actions root). The migration never moves the token or its directory. Python harnesses and job runners keep reading the same path. |
| Agent bus single-instance guard | The `agent-bus-host.lock` used to live inside the task-worker-actions root. r219 takes the same exclusive lock (same owner / private-file checks) at `<runtime>/task-service/agent-bus-host.lock` before binding. |
| `AgentMessageKind::TaskServiceSystem` (`"task_service_system"`, label `task_service`) | Kept **decode-only**: `runtime/management-v2/agent-bus-message-state.json` is decoded as a whole and may contain old task envelopes. Never produced; ordinary senders still cannot claim it. |
| Residual task messages | Startup redrive marks pending task-service records terminal (`task_service_removed`, not retryable) instead of re-queueing them. The bridge ACKs any task-service message it still sees without native submission. |
| Activity projector state (`runtime/management-v2/activity-projector-state.json`) | Pending `taskAssignmentActivity` / `taskWatchdogActivity` entries load as `activity: None` (the existing "intentional skip" value), so the projector keeps running. |
| Management journal | `cutex-methods-v2.json` keeps `director_rotate` in the three operation enums and `rotation_mode`, because retained journal events are re-validated on scan. The `taskService` conditions and `$defs` are removed (old task events fall back to the generic params object). |
| Agent Management state history | `AgentOperation(Kind)::DirectorRotate`, `rotation_mode`, operator-audit kinds from rotation, `HumanManagementProjectMutationKind::RepairDirectorSeat` and `ProjectAuditKind::DirectorSeatRepaired` stay **decode-only** so recorded receipts / audits still load. New requests with these are refused with `tool_removed`. An in-flight `director_rotate` action found on recovery is finished as failed (`director_rotate_removed`). |
| `/api/task*`, `/api/rotation/v1/release` (bus) and `/v2/task-service/*`, `/v2/release-rotation*`, `/v2/tasks/*`, `/v2/sessions/*/tasks`, `/v2/projects/*/tasks` (management) | One-release stubs: HTTP 410 with error code `tool_removed`, answered before auth and before any state access. Remove in r220. |
| MCP tools | `cutex_task_service`, `_read`, `_director`, `_terminal` keep their `tool_removed` tombstones; `director_rotate` gets one too, and `cutex_agent_management {"operation":"director_rotate"}` is refused with `tool_removed`. |
| Delegations / asks | Untouched. |
| AM project authority (`create_project`, `assign_director`, `clear_director`, `agent-authority`, `legacy-director-ownership-import`) | Kept; seat reads / writes removed. Director role (tool listing, owner-notify `directors`, project managers) comes only from `ProjectAuthority.authorized_director_session`. |
| Old transcripts with Task Service deliveries | Daemon never parses them back (external-input envelopes and views are generic). Desktop keeps the `task-service` module's timeline rows (`TaskReceivedRow`, `TaskCallRow`). |
| Webhook config `~/.cutex/notifications/outbound.json` | Retired event names `task.review_ready` / `task.closed` are dropped on read instead of failing validation. |
| Notification-level owner settings | Unknown event ids are already kept; removed registry entries do not invalidate saved settings. |
| `CodezConfig` keys `task_watchdog`, `owner_task_read_credentials`, `task_sort` | No `deny_unknown_fields`: old `config.json` loads; the keys are dropped on the next save. |
| `session-activity-state.json` `taskTurnBindings` | Ignored on read, dropped on next save. |
| Management v2 session delete response `openAssignments` | Kept as an always-empty array for one release. |

## 3. Removed / changed wire surfaces

- Capabilities (`/v2/capabilities`): remove `session-tasks` / `sessionTasks` and `task-detail` /
  `taskDetail`; add **`task-service-removed` / `taskServiceRemoved`**.
- Management events no longer emitted: `cutex/taskService/assignmentCommitted`,
  `…/communicationRecorded`, `…/assignmentTransitionCommitted`, `…/assignmentUpdated`,
  `cutex/taskWatchdog/firstStale`, `…/directorEscalated`.
- `thread/cutexActivity` items no longer produced: `taskAssignmentActivity`, `taskWatchdogActivity`.
- Owner notices no longer produced: `preActivationSkipped`.
- Error codes that disappear with their routes: `assignment_not_found`, `task_action_conflict`,
  `task_recovery_unavailable`, `task_store_unavailable`, `management_task_query_unavailable`,
  `seat_occupancy_*`, `release_template_*`, `release_rotation_*`, `archive_blocked_by_director_seat`,
  `archive_blocked_by_open_assignments`, `project_has_active_tasks`,
  `archive_requires_active_task_resolution`.
- CLI: `cutex agent task-action`, `cutex agent release-rotation`, `cutex agent manage
  director-rotate`, `cutex management seat …`, `cutex management release-rotation …`, `cutex human
  tasks …`, `cutex human stop|restart --cancel-tasks`; `cutex human doctor` loses `task_storage`.
- TUI: Tasks tab (Alt+4) and task reports go; the Alarms view that lived inside Tasks takes the
  Alt+4 slot. Settings lose "Task sort" and the "Task Watchdog" section.
- External recovery: the persisted review `authority_sha256` no longer covers seat state; a review
  prepared on r218 is reported stale and has to be prepared again.

## 4. One-time migration (`r219-removed`)

Runs at the start of `cutex management serve` and of the agent bus server, under an exclusive lock
`~/.cutex/archive/r219-removed/.migration.lock`. Each step is idempotent and logged to stderr and to
`~/.cutex/archive/r219-removed/migration.json` (step, source, destination, time, outcome).

| Source (under `~/.cutex`) | Destination |
|---|---|
| `runtime/task-service/task-worker-actions-v1/` (task provider SQLite + WAL, watchdog store, evidence store, old bus host lock) | `archive/r219-removed/runtime/task-service/task-worker-actions-v1/` |
| `runtime/task-service/seat-authority-v1/` (seat occupancy, Director seat transfers, release rotation receipts) | `archive/r219-removed/runtime/task-service/seat-authority-v1/` |
| `runtime/release-rotation-v1/` (release template) | `archive/r219-removed/runtime/release-rotation-v1/` |
| `runtime/agent-management/v1/task-service-seat-authority-v1/` (only exists for non-default roots) | `archive/r219-removed/runtime/agent-management/v1/task-service-seat-authority-v1/` |

Rules: never touches `runtime/task-service/job-service-completion.token`; a missing source is a
no-op; an existing destination gets a `.<UTC timestamp>` suffix instead of being overwritten; a move
that fails (for example a file still held open by an old r218 process on Windows) is logged and
retried on the next start, it does not block startup. Nothing is deleted. `archive/` record
retention only prunes `records-YYYY-MM.jsonl.gz` files, so the subdirectory is safe.

## 5. Desktop (`cutex-desktop`, branch `feat/r219-cleanup`)

- Remove `projectTasks` / `taskDetail` IPC + daemon calls, the store `tasks` slice and the
  `cutex/taskService/assignmentUpdated` handler, `TasksPage` / `TaskList`, the legacy
  「历史任务（旧）」 fold and the TaskList fallback in the delegations list.
- `task-service` module keeps only its old-transcript timeline rows (no page).
- Mocks: capability `task-service-removed` instead of `task-detail`.
- The disabled 轮换主管 button stays as is.

## 6. Tests

- Baseline at `a567c27` (private home, `--skip writers_serialize_and_stale_cas_does_not_plan_or_write
  --skip ui_contract_production_durable_import_`): 101 failures (84 lib incl. flaky
  `services::supervisor::tests::managed_lifecycle_and_open_starts_stopped`, 15 bin, 2 integration).
  42 of them are `task_service::provider::tests`, 17 `rotation::*`, 14 `role_revision::*`; those
  disappear with the deleted code.
- Generic tests that were built on task fixtures are rewritten, not deleted: integration-event
  dedupe / retention (now on neutral `cutex/test/*` methods), agent-bus state spill (alarm source),
  owner-queue and Job Service bus routes, live-over-catch-up activity projection.
- New: removed routes answer 410 `tool_removed` before any write; migration moves the stores, keeps
  the token, and is a no-op on the second run; redrive finalizes task-service records.
- `tests/*.py` (private Linux campaign harnesses with hard-coded owner-specific absolute paths) are not run on
  Windows; see §10.

## 7. Agent / skill side

- Delete `skills/cutex-rotate-director/`; update `skills/README.md` and
  `skills/cutex-manage-agents/SKILL.md`. Installed copies under `~/.cutex/codex-home/skills` and
  native homes are an owner step.
- Running agents keep a cached `tools/list` until relaunch; a call to `director_rotate` or a
  `cutex_task_service*` tool answers `tool_removed`.

## 8. Deploy and rollback notes

Back up before activation (services stopped):

- the whole `~/.cutex/runtime/task-service/` (incl. the token), `~/.cutex/runtime/release-rotation-v1/`;
- `~/.cutex/runtime/agent-management/` and `~/.cutex/runtime/management-v2/` (journal, activity
  projector state, agent-bus message state, session activity state);
- `~/.cutex/config.json` (holds `task_watchdog` / `owner_task_read_credentials`, dropped on next
  save) and `~/.cutex/notifications/outbound.json`;
- the session store with `snapshot-sessions-db.py` / sqlite `.backup` (never copy db + wal + shm of a
  running home).

Also record the list of agents active in the last 7 days (they may hold cached tool lists or open
task assignments that silently end with r219).

Before activation, also check:

- a Linux build + test run (Linux host): unix-only code was only compiled on Windows as far as `cfg`
  allows (`agent_management/migration.rs` authority digest, unix tests in `management_context`,
  app-server socket tests);
- the live AM store for `director_rotate` actions without a response: on recovery an effect-free one
  is finished as failed; any other one becomes an owner-action-required failure that keeps blocking
  its successor name / cwd until the owner resolves it;
- external-recovery and archive reviews prepared on r218 report stale (digests no longer cover seat
  state) and have to be prepared again;
- installed copies of the `cutex-rotate-director` skill (`~/.cutex/codex-home/skills`, native homes).

Rollback = stop services, restore the backup above, point the binaries back at r218. r218 cannot
use a home after r219 ran: the seat store and task provider are archived (r218 would recreate them
empty), `config.json` may have lost the task keys, and r219-written journal / AM records are not
guaranteed to be readable by r218. Moving the archived directories back by hand is possible but
not supported.

## 9. Implementation record

Branch `feat/r219-cleanup` (not pushed, not merged, not deployed), on top of `a567c27`:

| Commit | Content |
|---|---|
| `9befdce`, `586b4cf` | this removal map; README / reference docs, banners on the old task docs |
| `10377ef` | Release rotation and `director_rotate` (incl. `src/rotation`, `cli_app/rotation.rs`, the skill) |
| `516d3bc`, `035f01d` | `r219_migration` + running it first in both serve commands; `sha256_bytes` → `role_revision` |
| `f248ff4` | TUI: Tasks panel, task reports, Task Watchdog / Task sort settings; Alarms = Alt+4 |
| `d9152b1` (merge `131a040`) | management: 410 routes, owner task reads, seat administration, CLI, capabilities |
| `feca096` (merge `c77fa23`) | Agent Management: Director = project authority only, no Task Service checks |
| `17cda1c` | agent bus + app-server Task Service runtime (worker actions, delivery, watchdog, A4 facts) |
| `fbb93fb` | delete `src/task_service`, `src/task_delivery`, `src/seat`, task reports, task events / turn bindings / schema `$defs` |
| `a318414` | retire `system.preActivationSkipped` |
| `fe8c597` | `role_revision` trimmed to the shared newtypes (the Role-Seat v1 model had no caller even at r218) |

Lines (`git diff --numstat a567c27`): **+6.5k / −65.7k** in 150 files. `task_service` −18.2k,
`task_delivery` −5.2k, `rotation` −3.0k, `seat` −1.8k, `role_revision` −9.4k, agent bus +2.3k / −12.9k,
app-server +0.6k / −3.9k, management +0.4k / −3.2k, Agent Management +1.3k / −3.2k, CLI / TUI
+0.9k / −4.5k, migration +0.5k, docs / skills +0.3k / −0.2k.

Tests (Windows, private home, same skips as the baseline):

| | baseline `a567c27` | r219 |
|---|---|---|
| lib | 1635 passed, 84 failed, 15 ignored | 1477 passed, 11 failed, 13 ignored |
| bin | 721 passed, 15 failed, 5 ignored | 699 passed, 14 failed, 5 ignored |
| integration | 2 failed (`session_archive_cli_error_tests`) | the same 2 |

No new failure. The 74 baseline failures that are gone belonged to deleted code (42
`task_service`, 17 `rotation`, 14 `role_revision`, 1 `session_tui_tasks`); the remaining 27 are all
in the baseline list.

Dev-daemon smoke (2026-10-10, debug build, copy of the r218 e2e home, management 24570 / bus
24560): first start logged and recorded three moves (`task-worker-actions-v1`,
`seat-authority-v1`, `release-rotation-v1`) in `archive/r219-removed/migration.json`; the token
stayed in place with the same digest and the bus lock moved to `runtime/task-service/`; 8 sessions
listed, 4 projects with their Directors from project authority, a project opened (detail + memory);
all removed management and bus routes answered 410 `tool_removed` (also without auth),
`/v2/projects/{id}/tasks` too; capabilities have `task-service-removed` and neither
`session-tasks` nor `task-detail`; the r218 `outbound.json` with `task.review_ready` /
`task.closed` loaded without them; notification levels list no task events. Second start: no
moves, `migration.json` unchanged.

Desktop (`cutex-desktop` `feat/r219-cleanup`): `dcfe2cb` (Task Service UI out, timeline rows kept),
`0b43def` (docs). `pnpm -r typecheck` clean; vitest 1694 passed / 30 skipped.

## 10. Follow-ups

- r220: drop the 410 stubs and the MCP tombstones; consider dropping the decode-only variants after
  the retained journal / AM history no longer contains them.
- Python campaign harnesses under `tests/*.py` that exercise Task Service / seats / rotation
  (`external_delivery_cutpoints.py`, `soon_task_*`, `soon_cli_*`, `protected_management_*`,
  `external_delivery_*`, `external_recovery_history.py`, `stock_*_mcp_boundary.py`,
  `same_id_maintenance_composition.py`, `scripts/project_recent14_inventory.py`) need adapting on
  the Linux host before the next Linux campaign.
- Docs in other repos (desktop `TODO.md` / `PROGRESS.md`) are updated by the lead.
